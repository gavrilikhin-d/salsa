use syn::spanned::Spanned;
use syn::{ItemFn, ReturnType};

use crate::configuration::{self, Configuration, CycleRecoveryStrategy};
use crate::options::Options;

// #[salsa::component(in Jar0)]
// fn my_func(db: &dyn Jar0Db, input1: u32, input2: u32) -> String {
//     format!("Hello, world")
// }

pub(crate) fn component(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = syn::parse_macro_input!(args as Args);
    let item_fn = syn::parse_macro_input!(input as ItemFn);
    match component_helper(args, item_fn) {
        Ok(v) => v,
        Err(e) => return e.into_compile_error().into(),
    }
}

fn component_helper(args: Args, item_fn: ItemFn) -> syn::Result<proc_macro::TokenStream> {
    let struct_item = configuration_struct(&item_fn);
    let configuration = fn_configuration(&args, &item_fn)?;
    let struct_item_ident = &struct_item.ident;
    let struct_ty: syn::Type = parse_quote!(#struct_item_ident);
    let configuration_impl = configuration.to_impl(&struct_ty);
    let ingredients_for_impl = ingredients_for_impl(&args, &struct_ty);
    let (getter, item_impl) = wrapper_fns(&args, &item_fn, &struct_ty)?;

    Ok(proc_macro::TokenStream::from(quote! {
        #struct_item
        #configuration_impl
        #ingredients_for_impl
        #getter
        #item_impl
    }))
}

struct Component;

type Args = Options<Component>;

impl crate::options::AllowedOptions for Component {
    const RETURN_REF: bool = true;

    const NO_EQ: bool = true;

    const JAR: bool = true;

    const DATA: bool = false;

    const DB: bool = false;
}

fn configuration_struct(item_fn: &syn::ItemFn) -> syn::ItemStruct {
    let fn_name = item_fn.sig.ident.clone();
    let vis = &item_fn.vis;
    parse_quote! {
        #[allow(non_camel_case_types)]
        #vis struct #fn_name {
            function: salsa::function::FunctionIngredient<Self>,
        }
    }
}

fn fn_configuration(args: &Args, item_fn: &syn::ItemFn) -> syn::Result<Configuration> {
    let jar_ty = args.jar_ty();
    let key_ty = arg_ty(item_fn)?.clone();
    let value_ty = configuration::value_ty(&item_fn.sig);

    // FIXME: these are hardcoded for now
    let cycle_strategy = CycleRecoveryStrategy::Panic;

    let backdate_fn = configuration::should_backdate_value_fn(args.should_backdate());
    let recover_fn = configuration::panic_cycle_recovery_fn();

    // The type of the configuration struct; this has the same name as the fn itself.
    let fn_ty = item_fn.sig.ident.clone();

    // Make a copy of the fn with a different name; we will invoke this from `execute`.
    // We need to change the name because, otherwise, if the function invoked itself
    // recursively it would not go through the query system.
    let inner_fn_name = &syn::Ident::new("__fn", item_fn.sig.ident.span());
    let mut inner_fn = item_fn.clone();
    inner_fn.sig.ident = inner_fn_name.clone();

    // Create the `execute` function, which invokes the function itself (which we embed within).
    let execute_fn = parse_quote! {
        fn execute(__db: &salsa::function::DynDb<Self>, __id: Self::Key) -> Self::Value {
            #inner_fn

            let (__jar, __runtime) = <_ as salsa::storage::HasJar<#jar_ty>>::jar(__db);
            let __ingredients =
                <_ as salsa::storage::HasIngredientsFor<#fn_ty>>::ingredient(__jar);
            #inner_fn_name(__db, __id)
        }
    };

    Ok(Configuration {
        jar_ty,
        key_ty,
        value_ty,
        cycle_strategy,
        backdate_fn,
        execute_fn,
        recover_fn,
    })
}

fn ingredients_for_impl(args: &Args, struct_ty: &syn::Type) -> syn::ItemImpl {
    let jar_ty = &args.jar_ty;
    parse_quote! {
        impl salsa::storage::IngredientsFor for #struct_ty {
            type Ingredients = Self;
            type Jar = #jar_ty;

            fn create_ingredients<DB>(ingredients: &mut salsa::routes::Ingredients<DB>) -> Self::Ingredients
            where
                DB: salsa::DbWithJar<Self::Jar> + salsa::storage::JarFromJars<Self::Jar>,
            {
                Self {
                    function: {
                        let index = ingredients.push(|jars| {
                            let jar = <DB as salsa::storage::JarFromJars<Self::Jar>>::jar_from_jars(jars);
                            let ingredients =
                                <_ as salsa::storage::HasIngredientsFor<Self::Ingredients>>::ingredient(jar);
                            &ingredients.function
                        });
                        salsa::function::FunctionIngredient::new(index)
                    },
                }
            }
        }
    }
}

fn wrapper_fns(
    args: &Args,
    item_fn: &syn::ItemFn,
    struct_ty: &syn::Type,
) -> syn::Result<(syn::ItemFn, syn::ItemImpl)> {
    // The "getter" has same signature as the original:
    let getter_fn = getter_fn(args, item_fn, struct_ty)?;

    let ref_getter_fn = ref_getter_fn(args, item_fn, struct_ty)?;
    let accumulated_fn = accumulated_fn(args, item_fn, struct_ty)?;
    let setter_fn = setter_fn(args, item_fn, struct_ty)?;

    let item_impl: syn::ItemImpl = parse_quote! {
        impl #struct_ty {
            #ref_getter_fn
            #setter_fn
            #accumulated_fn
        }
    };

    Ok((getter_fn, item_impl))
}

fn getter_fn(
    args: &Args,
    item_fn: &syn::ItemFn,
    struct_ty: &syn::Type,
) -> syn::Result<syn::ItemFn> {
    let mut getter_fn = item_fn.clone();
    let arg_idents: Vec<_> = item_fn
        .sig
        .inputs
        .iter()
        .map(|arg| -> syn::Result<syn::Ident> {
            match arg {
                syn::FnArg::Receiver(_) => Err(syn::Error::new(arg.span(), "unexpected receiver")),
                syn::FnArg::Typed(pat_ty) => Ok(match &*pat_ty.pat {
                    syn::Pat::Ident(ident) => ident.ident.clone(),
                    _ => return Err(syn::Error::new(arg.span(), "unexpected receiver")),
                }),
            }
        })
        .collect::<Result<_, _>>()?;
    if args.return_ref.is_some() {
        getter_fn = make_fn_return_ref(getter_fn)?;
        getter_fn.block = Box::new(parse_quote_spanned! {
            item_fn.block.span() => {
                #struct_ty::get(#(#arg_idents,)*)
            }
        });
    } else {
        getter_fn.block = Box::new(parse_quote_spanned! {
            item_fn.block.span() => {
                Clone::clone(#struct_ty::get(#(#arg_idents,)*))
            }
        });
    }
    Ok(getter_fn)
}

fn ref_getter_fn(
    args: &Args,
    item_fn: &syn::ItemFn,
    struct_ty: &syn::Type,
) -> syn::Result<syn::ItemFn> {
    let jar_ty = &args.jar_ty;
    let mut ref_getter_fn = item_fn.clone();
    ref_getter_fn.sig.ident = syn::Ident::new("get", item_fn.sig.ident.span());
    ref_getter_fn = make_fn_return_ref(ref_getter_fn)?;

    let (db_var, arg_names) = fn_args(item_fn)?;
    ref_getter_fn.block = parse_quote! {
        {
            let (__jar, __runtime) = <_ as salsa::storage::HasJar<#jar_ty>>::jar(#db_var);
            let __ingredients = <_ as salsa::storage::HasIngredientsFor<#struct_ty>>::ingredient(__jar);
            __ingredients.function.fetch(#db_var, #(#arg_names),*)
        }
    };

    Ok(ref_getter_fn)
}

fn setter_fn(
    args: &Args,
    item_fn: &syn::ItemFn,
    struct_ty: &syn::Type,
) -> syn::Result<syn::ImplItemMethod> {
    // The setter has *always* the same signature as the original:
    // but it takes a value arg and has no return type.
    let jar_ty = &args.jar_ty;
    let (db_var, arg_names) = fn_args(item_fn)?;
    let mut setter_sig = item_fn.sig.clone();
    let value_ty = configuration::value_ty(&item_fn.sig);
    setter_sig.ident = syn::Ident::new("set", item_fn.sig.ident.span());
    let value_arg = syn::Ident::new("__value", item_fn.sig.output.span());
    setter_sig.inputs.push(parse_quote!(#value_arg: #value_ty));
    setter_sig.output = ReturnType::Default;
    Ok(syn::ImplItemMethod {
        attrs: vec![],
        vis: item_fn.vis.clone(),
        defaultness: None,
        sig: setter_sig,
        block: parse_quote! {
            {
                let (__jar, __runtime) = <_ as salsa::storage::HasJar<#jar_ty>>::jar(#db_var);
                let __ingredients = <_ as salsa::storage::HasIngredientsFor<#struct_ty>>::ingredient(__jar);
                __ingredients.function.set(#db_var, #(#arg_names),*, #value_arg)
            }
        },
    })
}

fn make_fn_return_ref(mut ref_getter_fn: syn::ItemFn) -> syn::Result<syn::ItemFn> {
    // The 0th input should be a `&dyn Foo`. We need to ensure
    // it has a named lifetime parameter.
    let (db_lifetime, _) = db_lifetime_and_ty(&mut ref_getter_fn)?;

    let (right_arrow, elem) = match ref_getter_fn.sig.output {
        ReturnType::Default => (
            syn::Token![->](ref_getter_fn.sig.paren_token.span),
            parse_quote!(()),
        ),
        ReturnType::Type(rarrow, ty) => (rarrow, ty),
    };

    let ref_output = syn::TypeReference {
        and_token: syn::Token![&](right_arrow.span()),
        lifetime: Some(db_lifetime),
        mutability: None,
        elem,
    };

    ref_getter_fn.sig.output = syn::ReturnType::Type(right_arrow, Box::new(ref_output.into()));

    Ok(ref_getter_fn)
}

fn db_lifetime_and_ty(func: &mut syn::ItemFn) -> syn::Result<(syn::Lifetime, &syn::Type)> {
    match &mut func.sig.inputs[0] {
        syn::FnArg::Receiver(r) => {
            return Err(syn::Error::new(r.span(), "expected database, not self"))
        }
        syn::FnArg::Typed(pat_ty) => match &mut *pat_ty.ty {
            syn::Type::Reference(ty) => match &ty.lifetime {
                Some(lt) => Ok((lt.clone(), &pat_ty.ty)),
                None => {
                    let and_token_span = ty.and_token.span();
                    let ident = syn::Ident::new("__db", and_token_span);
                    func.sig.generics.params.insert(
                        0,
                        syn::LifetimeDef {
                            attrs: vec![],
                            lifetime: syn::Lifetime {
                                apostrophe: and_token_span,
                                ident: ident.clone(),
                            },
                            colon_token: None,
                            bounds: Default::default(),
                        }
                        .into(),
                    );
                    let db_lifetime = syn::Lifetime {
                        apostrophe: and_token_span,
                        ident,
                    };
                    ty.lifetime = Some(db_lifetime.clone());
                    Ok((db_lifetime, &pat_ty.ty))
                }
            },
            _ => {
                return Err(syn::Error::new(
                    pat_ty.span(),
                    "expected database to be a `&` type",
                ))
            }
        },
    }
}

fn accumulated_fn(
    args: &Args,
    item_fn: &syn::ItemFn,
    struct_ty: &syn::Type,
) -> syn::Result<syn::ItemFn> {
    let jar_ty = &args.jar_ty;

    let mut accumulated_fn = item_fn.clone();
    accumulated_fn.sig.ident = syn::Ident::new("accumulated", item_fn.sig.ident.span());
    accumulated_fn.sig.generics.params.push(parse_quote! {
        __A: salsa::accumulator::Accumulator
    });
    accumulated_fn.sig.output = parse_quote! {
        -> Vec<<__A as salsa::accumulator::Accumulator>::Data>
    };

    let (db_lifetime, _) = db_lifetime_and_ty(&mut accumulated_fn)?;
    let predicate: syn::WherePredicate = parse_quote!(<#jar_ty as salsa::jar::Jar<#db_lifetime>>::DynDb: salsa::storage::HasJar<<__A as salsa::accumulator::Accumulator>::Jar>);

    if let Some(where_clause) = &mut accumulated_fn.sig.generics.where_clause {
        where_clause.predicates.push(predicate);
    } else {
        accumulated_fn.sig.generics.where_clause = parse_quote!(where #predicate);
    }

    let (db_var, arg_names) = fn_args(item_fn)?;
    accumulated_fn.block = parse_quote! {
        {
            let (__jar, __runtime) = <_ as salsa::storage::HasJar<#jar_ty>>::jar(#db_var);
            let __ingredients = <_ as salsa::storage::HasIngredientsFor<#struct_ty>>::ingredient(__jar);
            __ingredients.function.accumulated::<__A>(#db_var, #(#arg_names),*)
        }
    };

    Ok(accumulated_fn)
}

fn fn_args(item_fn: &syn::ItemFn) -> syn::Result<(proc_macro2::Ident, Vec<proc_macro2::Ident>)> {
    // Check that we have no receiver and that all argments have names
    if item_fn.sig.inputs.len() == 0 {
        return Err(syn::Error::new(
            item_fn.sig.span(),
            "method needs a database argument",
        ));
    }

    let mut input_names = vec![];
    for input in &item_fn.sig.inputs {
        match input {
            syn::FnArg::Receiver(r) => {
                return Err(syn::Error::new(r.span(), "no self argument expected"));
            }
            syn::FnArg::Typed(pat_ty) => match &*pat_ty.pat {
                syn::Pat::Ident(ident) => {
                    input_names.push(ident.ident.clone());
                }

                _ => {
                    return Err(syn::Error::new(
                        pat_ty.pat.span(),
                        "all arguments must be given names",
                    ));
                }
            },
        }
    }

    // Database is the first argument
    let db_var = input_names[0].clone();
    let arg_names = input_names[1..].to_owned();

    Ok((db_var, arg_names))
}

fn arg_ty(item_fn: &syn::ItemFn) -> syn::Result<&syn::Type> {
    // Check that we have no receiver and that all argments have names
    if item_fn.sig.inputs.len() != 2 {
        return Err(syn::Error::new(
            item_fn.sig.span(),
            "component method needs a database argument and an entity",
        ));
    }

    match &item_fn.sig.inputs[1] {
        syn::FnArg::Typed(pat_ty) => Ok(&pat_ty.ty),
        _ => {
            return Err(syn::Error::new(
                item_fn.sig.inputs[1].span(),
                "expected a fn parameter with a type",
            ));
        }
    }
}