use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Attribute, Ident, ItemFn, ItemStruct, LitStr, Token, Type, Visibility,
    parse::{Parse, ParseStream},
    spanned::Spanned,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a simple schema name from a `syn::Type` (e.g. `SendEmailResponse`
/// from `crate::model::email::SendEmailResponse`).
fn type_to_schema_name(ty: &Type) -> String {
    match ty {
        Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| quote! { #ty }.to_string()),
        _ => quote! { #ty }.to_string(),
    }
}

// ---------------------------------------------------------------------------
// ResponseEntry – a single entry in `responses = (T, (status = 200, schema = T, ...), ...)`
// ---------------------------------------------------------------------------

/// A response entry can be a bare type (status/description come from the type
/// via `#[openapi::response]`) or a full tuple with explicit metadata.
enum ResponseEntry {
    Full(ResponseDef),
    Bare(Type),
}

impl Parse for ResponseEntry {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Peek for parenthesized group → Full, otherwise → Bare
        if input.peek(syn::token::Paren) {
            input.parse().map(ResponseEntry::Full)
        } else {
            input.parse().map(ResponseEntry::Bare)
        }
    }
}

// ---------------------------------------------------------------------------
// ResponseDef – a single entry in `(status = 200, schema = T, description = "...")`
// ---------------------------------------------------------------------------

struct ResponseDef {
    /// Status as an expression: a numeric literal (`200`) or an enum constant
    /// (`Status::OK`). Emitted as `#status as u16` at codegen.
    status: syn::Expr,
    schema: Type,
    description: Option<String>,
}

impl Parse for ResponseDef {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        syn::parenthesized!(content in input);

        let mut status: Option<syn::Expr> = None;
        let mut schema: Option<Type> = None;
        let mut description: Option<String> = None;

        while !content.is_empty() {
            let key: Ident = content.parse()?;
            content.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "status" => {
                    let val: syn::Expr = content.parse()?;
                    status = Some(val);
                }
                "schema" => {
                    schema = Some(content.parse()?);
                }
                "description" => {
                    let val: LitStr = content.parse()?;
                    description = Some(val.value());
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("expected `status`, `schema`, or `description`, found `{key}`"),
                    ));
                }
            }

            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            status: status
                .ok_or_else(|| syn::Error::new(input.span(), "missing `status` in response"))?,
            schema: schema
                .ok_or_else(|| syn::Error::new(input.span(), "missing `schema` in response"))?,
            description,
        })
    }
}

// ---------------------------------------------------------------------------
// OpenApiArgs – parsed form of `#[endpoint(...)]`
// ---------------------------------------------------------------------------

struct OpenApiArgs {
    path: Option<String>,
    method: String,
    tag: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    deprecated: bool,
    request: Option<Type>,
    request_desc: Option<String>,
    responses: Vec<ResponseEntry>,
}

impl Parse for OpenApiArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut path: Option<String> = None;
        let mut method: Option<String> = None;
        let mut tag: Option<String> = None;
        let mut summary: Option<String> = None;
        let mut description: Option<String> = None;
        let mut deprecated = false;
        let mut request: Option<Type> = None;
        let mut request_desc: Option<String> = None;
        let mut responses: Vec<ResponseEntry> = Vec::new();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            let key_str = key.to_string();

            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;

                match key_str.as_str() {
                    "path" | "tag" | "summary" | "description" | "request_desc" => {
                        let val: LitStr = input.parse()?;
                        let val_str = val.value();
                        match key_str.as_str() {
                            "path" => path = Some(val_str),
                            "tag" => tag = Some(val_str),
                            "summary" => summary = Some(val_str),
                            "description" => description = Some(val_str),
                            "request_desc" => request_desc = Some(val_str),
                            _ => unreachable!(),
                        }
                    }
                    "method" => {
                        // Accept either `method = "post"` or `method = Method::POST`.
                        let val: syn::Expr = input.parse()?;
                        method = Some(method_from_expr(&val)?);
                    }
                    "request" => {
                        let ty: Type = input.parse()?;
                        request = Some(ty);
                    }
                    "responses" => {
                        let content;
                        syn::parenthesized!(content in input);
                        let parsed: syn::punctuated::Punctuated<ResponseEntry, Token![,]> =
                            content.parse_terminated(ResponseEntry::parse, Token![,])?;
                        responses = parsed.into_iter().collect();
                    }
                    _ => {
                        return Err(syn::Error::new(
                            key.span(),
                            format!("unknown key `{key_str}`"),
                        ));
                    }
                }
            } else {
                // Bare flag (no `= value`)
                match key_str.as_str() {
                    "deprecated" => deprecated = true,
                    _ => {
                        return Err(syn::Error::new(
                            key.span(),
                            format!("unknown flag `{key_str}`"),
                        ));
                    }
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            path,
            method: method.unwrap_or_else(|| "get".into()),
            tag,
            summary,
            description,
            deprecated,
            request,
            request_desc,
            responses,
        })
    }
}

/// Normalize a `method` value into the lowercase HTTP verb: `"post"` or
/// `Method::POST` both become `"post"`.
fn method_from_expr(expr: &syn::Expr) -> syn::Result<String> {
    match expr {
        syn::Expr::Lit(lit) => {
            if let syn::Lit::Str(s) = &lit.lit {
                Ok(s.value().to_lowercase())
            } else {
                Err(syn::Error::new(
                    expr.span(),
                    "expected a string like `\"post\"`",
                ))
            }
        }
        syn::Expr::Path(p) => {
            let seg = p
                .path
                .segments
                .last()
                .ok_or_else(|| syn::Error::new(expr.span(), "expected `Method::POST`"))?;
            Ok(seg.ident.to_string().to_lowercase())
        }
        _ => Err(syn::Error::new(
            expr.span(),
            "expected `\"post\"` or `Method::POST`",
        )),
    }
}

/// Normalize a `status` value into the numeric status code: `422` or
/// `Status::UNPROCESSABLE_ENTITY` both become `422`.
fn status_from_expr(expr: &syn::Expr) -> syn::Result<u16> {
    match expr {
        syn::Expr::Lit(lit) => {
            if let syn::Lit::Int(i) = &lit.lit {
                i.base10_parse()
            } else {
                Err(syn::Error::new(
                    expr.span(),
                    "expected a status code like `422` or `Status::UNPROCESSABLE_ENTITY`",
                ))
            }
        }
        syn::Expr::Path(p) => {
            let seg = p.path.segments.last().ok_or_else(|| {
                syn::Error::new(expr.span(), "expected `Status::UNPROCESSABLE_ENTITY`")
            })?;
            let name = seg.ident.to_string();
            let code = match name.as_str() {
                "OK" => 200,
                "CREATED" => 201,
                "ACCEPTED" => 202,
                "NO_CONTENT" => 204,
                "BAD_REQUEST" => 400,
                "UNAUTHORIZED" => 401,
                "FORBIDDEN" => 403,
                "NOT_FOUND" => 404,
                "CONFLICT" => 409,
                "UNPROCESSABLE_ENTITY" => 422,
                "TOO_MANY_REQUESTS" => 429,
                "INTERNAL_SERVER_ERROR" => 500,
                "NOT_IMPLEMENTED" => 501,
                "SERVICE_UNAVAILABLE" => 503,
                "GATEWAY_TIMEOUT" => 504,
                _ => {
                    return Err(syn::Error::new(
                        seg.span(),
                        format!("expected a `Status::*` constant, found `{name}`"),
                    ));
                }
            };
            Ok(code)
        }
        _ => Err(syn::Error::new(
            expr.span(),
            "expected `422` or `Status::UNPROCESSABLE_ENTITY`",
        )),
    }
}

// ---------------------------------------------------------------------------
// #[endpoint] attribute macro
// ---------------------------------------------------------------------------

/// Implementation of the `#[endpoint(...)]` attribute macro (see `lib.rs` for
/// the exported wrapper). Annotates an async handler function with OpenAPI
/// metadata and generates a hidden route-handle const.
pub(crate) fn endpoint(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args: OpenApiArgs = match syn::parse(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };

    let item_fn: ItemFn = match syn::parse(item) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };

    expand_endpoint(args, item_fn)
}

/// If the handler takes exactly one parameter of the form
/// `Context<Request, State>`, returns `Some((request_ty, state_ty))`.
fn parse_context_handler(
    inputs: &syn::punctuated::Punctuated<syn::FnArg, syn::token::Comma>,
) -> Option<(Type, Type)> {
    let mut args = inputs.iter();
    let arg = args.next()?;
    if args.next().is_some() {
        return None; // more than one parameter
    }
    let syn::FnArg::Typed(pat_type) = arg else {
        return None;
    };
    let Type::Path(type_path) = &*pat_type.ty else {
        return None;
    };
    let last = type_path.path.segments.last()?;
    if last.ident != "Context" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(generics) = &last.arguments else {
        return None;
    };
    let mut types = generics.args.iter().filter_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    let request = types.next()?;
    let state = types.next()?;
    if types.next().is_some() {
        return None;
    }
    Some((request, state))
}

/// `Method` variant name for an HTTP method string (e.g. `post` → `POST`).
fn request_method_ident(method_str: &str) -> Ident {
    format_ident!("{}", method_str.to_uppercase())
}

/// Generate the `.json_content_response(...)` calls for the response docs.
fn gen_response_calls(args: &OpenApiArgs) -> Vec<proc_macro2::TokenStream> {
    args.responses
        .iter()
        .map(|entry| match entry {
            ResponseEntry::Full(r) => {
                let status = &r.status;
                let schema_ty = &r.schema;
                let schema_name = type_to_schema_name(schema_ty);
                let desc = match &r.description {
                    Some(d) => quote! { #d },
                    None => {
                        quote! { <#schema_ty as ::sevria_service_kit::http::Endpoint>::description() }
                    }
                };
                quote! {
                    .json_content_response(&format!("{}", #status as u16), #desc, #schema_name)
                }
            }
            ResponseEntry::Bare(ty) => {
                let schema_name = type_to_schema_name(ty);
                quote! {
                    .json_content_response(
                        &format!("{}", <#ty>::__openapi_response_meta().0),
                        <#ty>::__openapi_response_meta().1,
                        #schema_name,
                    )
                }
            }
        })
        .collect()
}

/// Generate the OpenAPI documentation statements applied to `router`: path and
/// query parameters, optional request body, and the response entries.
fn gen_docs(args: &OpenApiArgs, method_ident: &Ident, path: &str) -> proc_macro2::TokenStream {
    let tag_call = args.tag.as_ref().map(|t| quote! { .tag(#t) });
    let summary_call = args.summary.as_ref().map(|s| quote! { .summary(#s) });
    let description_call = args
        .description
        .as_ref()
        .map(|d| quote! { .description(#d) });
    let deprecated_call = if args.deprecated {
        Some(quote! { .deprecated() })
    } else {
        None
    };
    let response_calls = gen_response_calls(args);
    let request_desc = args.request_desc.as_deref().unwrap_or("Request body");

    // OpenAPI documentation for the request: path/query parameters and the
    // JSON body are all derived from ONE request type's field metadata, where
    // each field is tagged with its source via `#[openapi(from = Source::X)]`.
    if let Some(req_ty) = &args.request {
        let body_schema_name = format!("{}Body", type_to_schema_name(req_ty));
        quote! {
            let __fields = <#req_ty as ::sevria_service_kit::http::Endpoint>::request_fields();
            let __body_schema_name = #body_schema_name;
            let __doc = ::sevria_service_kit::http::path::#method_ident(#path)
                #tag_call
                #summary_call
                #description_call
                #deprecated_call
                .parameters(
                    ::sevria_service_kit::http::parameters_from_fields(
                        &__fields,
                        ::sevria_service_kit::http::Source::Path,
                    )
                )
                .parameters(
                    ::sevria_service_kit::http::parameters_from_fields(
                        &__fields,
                        ::sevria_service_kit::http::Source::Query,
                    )
                );
            let __doc = if __fields
                .iter()
                .any(|f| f.source == ::sevria_service_kit::http::Source::Body)
            {
                __doc.json_request_with_schema(#request_desc, __body_schema_name)
            } else {
                __doc
            };
            router.describe(__doc #(#response_calls)*);
            if __fields
                .iter()
                .any(|f| f.source == ::sevria_service_kit::http::Source::Body)
            {
                router.add_schema(
                    __body_schema_name,
                    ::sevria_service_kit::http::body_schema_from_fields(&__fields),
                );
            }
        }
    } else {
        // Plain documentation for endpoints without a request type.
        quote! {
            router.describe(
                ::sevria_service_kit::http::path::#method_ident(#path)
                    #tag_call
                    #summary_call
                    #description_call
                    #deprecated_call
                    #(#response_calls)*
            );
        }
    }
}

/// Register one schema component per unique response schema type. (The request
/// body schema is registered separately inside the request docs block.)
fn gen_schema_registrations(args: &OpenApiArgs) -> Vec<proc_macro2::TokenStream> {
    let mut out = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in &args.responses {
        let (sn, schema_ty) = match entry {
            ResponseEntry::Full(r) => (type_to_schema_name(&r.schema), &r.schema),
            ResponseEntry::Bare(ty) => (type_to_schema_name(ty), ty),
        };
        if seen.insert(sn.clone()) {
            out.push(quote! {
                router.add_schema(#sn, <#schema_ty as ::sevria_service_kit::http::Endpoint>::json_schema());
            });
        }
    }
    out
}

/// Whether the endpoint returns a `Result<_, _>` (last path segment `Result`),
/// e.g. `Result<T, Error>` or the `sevria_service_kit::Result<T>` alias. Such
/// handlers are mapped to a status + `ErrorResponse` body instead of a plain
/// `200` JSON body.
fn is_result_return(sig: &syn::Signature) -> bool {
    let syn::ReturnType::Type(_, ty) = &sig.output else {
        return false;
    };
    let syn::Type::Path(tp) = &**ty else {
        return false;
    };
    tp.path
        .segments
        .last()
        .map(|s| s.ident == "Result")
        .unwrap_or(false)
}

fn expand_endpoint(mut args: OpenApiArgs, item_fn: ItemFn) -> TokenStream {
    let fn_name = &item_fn.sig.ident;
    let fn_vis = &item_fn.vis;

    let other_attrs: Vec<&Attribute> = item_fn
        .attrs
        .iter()
        .filter(|a| !a.path().is_ident("endpoint"))
        .collect();

    // A handler taking a single `Context<Request, State>` parameter receives
    // the parsed request plus injected state (dependency injection). The
    // request type is inferred from the `Context` when `request` is omitted.
    let context_state = parse_context_handler(&item_fn.sig.inputs);
    if let Some((request_ty, _)) = &context_state {
        if args.request.is_none() {
            args.request = Some(request_ty.clone());
        }
    }

    let register_fn_name = format_ident!("__openapi_register_{}", fn_name);
    let handle_const_name = format_ident!("{}_handle", fn_name);

    // Path defaults to `/fn_name` when `#[endpoint()]` has no explicit path.
    let path = args.path.clone().unwrap_or_else(|| format!("/{}", fn_name));
    let method_str = &args.method;
    let method_ident = format_ident!("{}", method_str);

    let docs = gen_docs(&args, &method_ident, &path);
    let schema_registrations = gen_schema_registrations(&args);

    let router_method = format_ident!("{}", method_str.to_lowercase());

    // `Result<_, Error>` endpoints map to a status + `ErrorResponse` body;
    // plain `Serialize` returns are wrapped in `Json` (HTTP 200).
    let is_result = is_result_return(&item_fn.sig);
    let wrapper = if is_result {
        quote! { ::sevria_service_kit::http::result_to_response }
    } else {
        quote! { ::sevria_service_kit::http::__private::Json }
    };

    // Route registration: with a `request = T` type the extractor wiring is
    // generated by the `Schema` derive (`T::__register_route`), which knows
    // each field's source. Without one, register the plain handler.
    let route_registration = if let Some((_, state_ty)) = &context_state {
        let req_ty = args.request.as_ref().expect(
            "`Context<Request, State>` handlers require a request type \
             (infer it or pass `request = T`)",
        );
        let request_method = request_method_ident(method_str);
        quote! {
            <#req_ty>::__register_route_with_state::<#state_ty, _, _, _>(
                router,
                #path,
                ::sevria_service_kit::http::Method::#request_method,
                move |__ctx: ::sevria_service_kit::http::Context<#req_ty, #state_ty>| async move {
                    #wrapper(#fn_name(__ctx).await)
                },
            );
        }
    } else if let Some(ref req_ty) = args.request {
        let request_method = request_method_ident(method_str);
        quote! {
            <#req_ty>::__register_route(
                router,
                #path,
                ::sevria_service_kit::http::Method::#request_method,
                move |__req: #req_ty| async move {
                    #wrapper(#fn_name(__req).await)
                },
            );
        }
    } else if is_result {
        quote! {
            router.add_route(
                #path,
                ::sevria_service_kit::http::__private::routing::#router_method(
                    move || async move {
                        ::sevria_service_kit::http::result_to_response(#fn_name().await)
                    }
                ),
            );
        }
    } else {
        quote! {
            router.#router_method(#path, #fn_name);
        }
    };

    let expanded = quote! {
        #(#other_attrs)*
        #fn_vis #item_fn

        #[doc(hidden)]
        #[allow(non_snake_case)]
        #fn_vis fn #register_fn_name(router: &mut ::sevria_service_kit::http::Router) {
            #route_registration
            #docs
            #(#schema_registrations)*
        }

        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        #fn_vis const #handle_const_name: ::sevria_service_kit::http::RouteHandle =
            ::sevria_service_kit::http::RouteHandle::new(#register_fn_name);
    };

    expanded.into()
}

// ---------------------------------------------------------------------------
// #[router] attribute macro
// ---------------------------------------------------------------------------

/// Implementation of the `#[router]` attribute macro (see `lib.rs` for the
/// exported wrapper). Turns an inherent `impl` block whose methods are
/// annotated with `#[endpoint(...)]` into a router builder.
///
/// Generates a `pub fn into_router(self) -> Router` method that registers every
/// endpoint's route, OpenAPI documentation, and schemas.
pub(crate) fn router(attr: TokenStream, item: TokenStream) -> TokenStream {
    // `#[router]` takes no attribute arguments.
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "`#[router]` takes no arguments",
        )
        .to_compile_error()
        .into();
    }

    let item_impl: syn::ItemImpl = match syn::parse(item) {
        Ok(i) => i,
        Err(e) => return e.to_compile_error().into(),
    };

    expand_router(item_impl)
}

/// Infer the request type of an endpoint method: the single typed parameter
/// after the receiver (`async fn greet(self, req: GreetingRequest)`).
fn infer_method_request(sig: &syn::Signature) -> syn::Result<Option<Type>> {
    let mut typed = sig.inputs.iter().filter_map(|a| match a {
        syn::FnArg::Typed(t) => Some(&*t.ty),
        _ => None,
    });
    let first = typed.next().cloned();
    if typed.next().is_some() {
        return Err(syn::Error::new(
            sig.span(),
            "endpoint methods take `self` and at most one request parameter; \
             specify `request = T` explicitly",
        ));
    }
    Ok(first)
}

/// Build the route + docs + schema registration statements for one endpoint
/// method, all emitted inside a single `{ ... }` block.
fn build_method_registration(
    method: &syn::ImplItemFn,
    args: &OpenApiArgs,
) -> syn::Result<proc_macro2::TokenStream> {
    // Endpoint methods must take the receiver by value so the generated route
    // closure can clone the router struct and call the method per request.
    let receiver = method.sig.receiver();
    let consumes_self = match receiver {
        Some(syn::Receiver {
            kind: syn::ReceiverKind::Value,
            ..
        }) => true,
        _ => false,
    };
    if !consumes_self {
        return Err(syn::Error::new(
            receiver
                .map(|r| r.span())
                .unwrap_or_else(|| method.sig.span()),
            "`#[endpoint]` methods must take `self` by value",
        ));
    }

    let fn_name = &method.sig.ident;
    let path = args.path.clone().unwrap_or_else(|| format!("/{}", fn_name));
    let method_str = &args.method;
    let method_ident = format_ident!("{}", method_str);
    let request_method = request_method_ident(method_str);

    // Request type: explicit `request = T` or inferred from the method
    // signature.
    let request = if let Some(t) = &args.request {
        Some(t.clone())
    } else {
        infer_method_request(&method.sig)?
    };

    // `Result<_, Error>` endpoints map to a status + `ErrorResponse` body;
    // plain `Serialize` returns are wrapped in `Json` (HTTP 200).
    let is_result = is_result_return(&method.sig);
    let wrapper = if is_result {
        quote! { ::sevria_service_kit::http::result_to_response }
    } else {
        quote! { ::sevria_service_kit::http::__private::Json }
    };

    let route_reg = match &request {
        Some(req_ty) => quote! {
            <#req_ty>::__register_route(
                &mut router,
                #path,
                ::sevria_service_kit::http::Method::#request_method,
                {
                    let this = self.clone();
                    move |__req: #req_ty| {
                        let this = this.clone();
                        async move { #wrapper(this.#fn_name(__req).await) }
                    }
                },
            );
        },
        None => {
            let router_method = format_ident!("{}", method_str.to_lowercase());
            if is_result {
                quote! {
                    router.add_route(
                        #path,
                        ::sevria_service_kit::http::__private::routing::#router_method(
                            {
                                let this = self.clone();
                                move || {
                                    let this = this.clone();
                                    async move {
                                        ::sevria_service_kit::http::result_to_response(this.#fn_name().await)
                                    }
                                }
                            },
                        ),
                    );
                }
            } else {
                quote! {
                    router.#router_method(
                        #path,
                        {
                            let this = self.clone();
                            move || {
                                let this = this.clone();
                                async move { this.#fn_name().await }
                            }
                        },
                    );
                }
            }
        }
    };

    let docs = gen_docs(args, &method_ident, &path);
    let schema_registrations = gen_schema_registrations(args);

    Ok(quote! {
        {
            #route_reg
            #docs
            #(#schema_registrations)*
        }
    })
}

fn expand_router(item_impl: syn::ItemImpl) -> TokenStream {
    let attrs = &item_impl.attrs;
    let defaultness = &item_impl.modifiers.defaultness;
    let polarity = &item_impl.modifiers.polarity;
    let unsafety = &item_impl.unsafety;
    let generics = &item_impl.generics;
    let self_ty = &item_impl.self_ty;
    let where_clause = &item_impl.generics.where_clause;

    let mut registrations: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut items: Vec<syn::ImplItem> = Vec::new();

    for item in item_impl.items {
        match item {
            syn::ImplItem::Fn(method) => {
                if method.attrs.iter().any(|a| a.path().is_ident("endpoint")) {
                    // Parse the `#[endpoint(...)]` arguments.
                    let attr = method
                        .attrs
                        .iter()
                        .find(|a| a.path().is_ident("endpoint"))
                        .expect("checked above");
                    let args: OpenApiArgs = match attr.parse_args() {
                        Ok(a) => a,
                        Err(e) => return e.to_compile_error().into(),
                    };
                    match build_method_registration(&method, &args) {
                        Ok(reg) => registrations.push(reg),
                        Err(e) => return e.to_compile_error().into(),
                    }

                    // Keep the method but strip the consumed `#[endpoint]` attr.
                    let mut method = method;
                    method.attrs.retain(|a| !a.path().is_ident("endpoint"));
                    items.push(syn::ImplItem::Fn(method));
                } else {
                    items.push(syn::ImplItem::Fn(method));
                }
            }
            other => items.push(other),
        }
    }

    let into_router = quote! {
        /// Build the router: registers every `#[endpoint]` route plus its
        /// OpenAPI metadata. The router struct must be `Clone` so each route
        /// can capture a copy of it.
        pub fn into_router(self) -> ::sevria_service_kit::http::Router {
            let mut router = ::sevria_service_kit::http::Router::new();
            #(#registrations)*
            router
        }
    };

    quote! {
        #(#attrs)*
        #defaultness #unsafety impl #polarity #generics #self_ty #where_clause {
            #(#items)*
            #into_router
        }
    }
    .into()
}

// ---------------------------------------------------------------------------
// #[derive(Schema)] – implements Endpoint
// ---------------------------------------------------------------------------

/// Implementation of the `#[derive(Schema)]` macro (see `lib.rs` for the
/// exported wrapper). Implements [`Endpoint`] for a type.
///
/// Reads the `#[openapi(description = "...")]` attribute on the struct.
pub(crate) fn derive_schema(item: TokenStream) -> TokenStream {
    let input: ItemStruct = match syn::parse(item) {
        Ok(s) => s,
        Err(e) => return e.to_compile_error().into(),
    };

    expand_schema(input)
}

/// Whether the request type is validated by `garde`, i.e. it also derives
/// `garde::Validate`. When true, generated route handlers validate the
/// deserialized request and return `422` on failure before the endpoint
/// handler runs.
///
/// The compiler strips the `#[derive(...)]` attribute before invoking a derive
/// macro, so `Schema` can never see `Validate` in the derive list. Instead we
/// rely on the `#[garde(...)]` attributes: they are passed through, and since
/// garde's derive registers `attributes(garde)`, such an attribute only
/// compiles when the type actually derives `Validate`. An explicit
/// `#[schema(validate)]` on the struct is also accepted as an opt-in.
fn derives_validate(input: &ItemStruct) -> bool {
    // Explicit opt-in: `#[schema(validate)]`.
    let has_schema_validate = input.attrs.iter().any(|attr| {
        if !attr.path().is_ident("schema") {
            return false;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("validate") {
                Ok(())
            } else {
                Err(meta.error("expected `validate`"))
            }
        })
        .is_ok()
    });
    if has_schema_validate {
        return true;
    }

    // Automatic: any `#[garde(...)]` attribute implies `#[derive(Validate)]`.
    let has_garde = |attrs: &[Attribute]| attrs.iter().any(|a| a.path().is_ident("garde"));
    if has_garde(&input.attrs) {
        return true;
    }
    match &input.fields {
        syn::Fields::Named(fields) => fields.named.iter().any(|f| has_garde(&f.attrs)),
        syn::Fields::Unnamed(fields) => fields.unnamed.iter().any(|f| has_garde(&f.attrs)),
        syn::Fields::Unit => false,
    }
}

/// The axum handler body for a request. When the request type derives
/// `garde::Validate`, the deserialized request is validated first; on failure a
/// `422` `ErrorResponse` is returned, so the endpoint handler only runs for
/// valid input. `build` constructs the request value; `state` is `Some(...)`
/// for the stateful variant, in which case the request is wrapped into a
/// `Context` before being passed to the handler.
fn handler_body(
    validates: bool,
    build: proc_macro2::TokenStream,
    state: Option<proc_macro2::TokenStream>,
) -> proc_macro2::TokenStream {
    if validates {
        let handler_arg = match &state {
            Some(state) => quote! {
                ::sevria_service_kit::http::Context::new(__request, #state)
            },
            None => quote! { __request },
        };
        quote! {
            let __request = #build;
            match ::sevria_service_kit::Validate::validate(&__request) {
                Ok(()) => ::sevria_service_kit::http::__private::IntoResponse::into_response(
                    handler(#handler_arg).await,
                ),
                Err(__report) => ::sevria_service_kit::http::validation_response(__report),
            }
        }
    } else {
        match &state {
            Some(state) => quote! {
                handler(::sevria_service_kit::http::Context::new(#build, #state)).await
            },
            None => quote! {
                handler(#build).await
            },
        }
    }
}

fn expand_schema(input: ItemStruct) -> TokenStream {
    let name = &input.ident;
    let vis = &input.vis;
    let generics = &input.generics;

    let validates = derives_validate(&input);

    let mut description: Option<String> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("schema") {
            continue;
        }

        let result: syn::Result<()> = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("description") {
                let val: LitStr = meta.value()?.parse()?;
                description = Some(val.value());
                Ok(())
            } else {
                Err(syn::Error::new(
                    meta.path.span(),
                    "expected `description = \"...\"`",
                ))
            }
        });

        if let Err(e) = result {
            return e.to_compile_error().into();
        }
    }

    let desc = description.unwrap_or_else(|| name.to_string());

    // Parse per-field metadata: source (`Path`/`Query`/`Body`), description,
    // example, and serde attrs.
    let field_infos = match parse_field_infos(&input.fields) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };

    // Full JSON Schema for the type (used for `responses` docs).
    let schema_body = build_json_schema(&field_infos);

    // Field-level request metadata used by `#[endpoint(request = T)]`.
    let request_fields_impl = request_fields_impl(&field_infos);

    // Inherent helpers: extractor route registration + request-part
    // sub-structs + merge. Only generated for non-generic structs; request
    // types must use `#[derive(Schema)]`.
    let request_machinery = if generics.params.is_empty() {
        request_machinery(name, vis, &field_infos, validates)
    } else {
        proc_macro2::TokenStream::new()
    };

    let expanded = quote! {
        impl #generics ::sevria_service_kit::http::Endpoint for #name #generics {
            fn description() -> &'static str {
                #desc
            }

            fn json_schema() -> ::sevria_service_kit::__private::serde_json::Value {
                #schema_body
            }

            #request_fields_impl
        }

        #request_machinery
    };

    expanded.into()
}

// ---------------------------------------------------------------------------
// Request field metadata (used by #[derive(Schema)])
// ---------------------------------------------------------------------------

/// Which part of the HTTP request a field belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Path,
    Query,
    Body,
}

/// Parsed metadata for a single struct field.
struct FieldInfo {
    ident: Ident,
    json_name: String,
    ty: Type,
    source: SourceKind,
    description: Option<String>,
    example: Option<String>,
    serde_attrs: Vec<Attribute>,
    required: bool,
}

fn parse_field_infos(fields: &syn::Fields) -> syn::Result<Vec<FieldInfo>> {
    fields
        .iter()
        .map(|field| {
            let ident = field.ident.clone().expect("named fields required");
            let json_name = serde_rename(field).unwrap_or_else(|| ident.to_string());
            let (source, description, example) = openapi_field_meta(field)?;
            let serde_attrs: Vec<Attribute> = field
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("serde"))
                .cloned()
                .collect();
            let required = !is_option_type(&field.ty);
            Ok(FieldInfo {
                ident,
                json_name,
                ty: field.ty.clone(),
                source,
                description,
                example,
                serde_attrs,
                required,
            })
        })
        .collect()
}

/// Parse `#[openapi(from = Source::X, description = "...", example = "...")]`
/// on a field. `from` defaults to [`SourceKind::Body`]; description falls back
/// to `///` doc comments.
fn openapi_field_meta(
    field: &syn::Field,
) -> syn::Result<(SourceKind, Option<String>, Option<String>)> {
    let mut source = SourceKind::Body;
    let mut description: Option<String> = None;
    let mut example: Option<String> = None;

    for attr in &field.attrs {
        if !attr.path().is_ident("schema") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("from") {
                let path: syn::Path = meta.value()?.parse()?;
                let last = path
                    .segments
                    .last()
                    .ok_or_else(|| syn::Error::new(meta.path.span(), "expected a source path"))?;
                source = match last.ident.to_string().as_str() {
                    "Path" => SourceKind::Path,
                    "Query" => SourceKind::Query,
                    "Body" => SourceKind::Body,
                    other => {
                        return Err(syn::Error::new(
                            last.span(),
                            format!(
                                "expected `Source::Path`, `Source::Query`, or `Source::Body`, found `{other}`"
                            ),
                        ));
                    }
                };
                Ok(())
            } else if meta.path.is_ident("description") {
                let val: LitStr = meta.value()?.parse()?;
                description = Some(val.value());
                Ok(())
            } else if meta.path.is_ident("example") {
                let val: LitStr = meta.value()?.parse()?;
                example = Some(val.value());
                Ok(())
            } else {
                Err(syn::Error::new(
                    meta.path.span(),
                    "expected `from`, `description`, or `example`",
                ))
            }
        })?;
    }

    if description.is_none() {
        description = doc_comment(field);
    }

    Ok((source, description, example))
}

/// Extract `///` doc comments from a field.
fn doc_comment(field: &syn::Field) -> Option<String> {
    let mut doc = String::new();
    for attr in &field.attrs {
        if attr.path().is_ident("doc") {
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if let syn::Expr::Lit(expr_lit) = &nv.value {
                    if let syn::Lit::Str(s) = &expr_lit.lit {
                        let text = s.value().trim().to_string();
                        if !text.is_empty() {
                            if !doc.is_empty() {
                                doc.push('\n');
                            }
                            doc.push_str(&text);
                        }
                    }
                }
            }
        }
    }
    if doc.is_empty() { None } else { Some(doc) }
}

/// Generate the `Endpoint::request_fields()` override.
fn request_fields_impl(field_infos: &[FieldInfo]) -> proc_macro2::TokenStream {
    let exprs = field_infos.iter().map(|fi| {
        let name = &fi.json_name;
        let source = match fi.source {
            SourceKind::Path => quote! { ::sevria_service_kit::http::Source::Path },
            SourceKind::Query => quote! { ::sevria_service_kit::http::Source::Query },
            SourceKind::Body => quote! { ::sevria_service_kit::http::Source::Body },
        };
        let schema = infer_json_type(&fi.ty);
        let required = fi.required;
        let description = fi
            .description
            .as_deref()
            .map(|d| quote! { Some(#d.to_string()) })
            .unwrap_or_else(|| quote! { None });
        let example = fi
            .example
            .as_deref()
            .map(|e| quote! { Some(::sevria_service_kit::__private::serde_json::json!(#e)) })
            .unwrap_or_else(|| quote! { None });
        quote! {
            ::sevria_service_kit::http::RequestField {
                name: #name.to_string(),
                source: #source,
                schema: #schema,
                required: #required,
                description: #description,
                example: #example,
            }
        }
    });

    quote! {
        fn request_fields() -> ::std::vec::Vec<::sevria_service_kit::http::RequestField> {
            ::std::vec![ #(#exprs),* ]
        }
    }
}

/// Generate the inherent request helpers: per-source `Deserialize` sub-structs,
/// a merge constructor, and `__register_route` which builds the axum closure.
fn request_machinery(
    name: &Ident,
    vis: &Visibility,
    field_infos: &[FieldInfo],
    validates: bool,
) -> proc_macro2::TokenStream {
    let path_fields: Vec<&FieldInfo> = field_infos
        .iter()
        .filter(|f| matches!(f.source, SourceKind::Path))
        .collect();
    let query_fields: Vec<&FieldInfo> = field_infos
        .iter()
        .filter(|f| matches!(f.source, SourceKind::Query))
        .collect();
    let body_fields: Vec<&FieldInfo> = field_infos
        .iter()
        .filter(|f| matches!(f.source, SourceKind::Body))
        .collect();

    let path_struct = format_ident!("{}__RequestPath", name);
    let query_struct = format_ident!("{}__RequestQuery", name);
    let body_struct = format_ident!("{}__RequestBody", name);

    let mut sub_structs: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut merge_params: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut merge_fields: Vec<proc_macro2::TokenStream> = Vec::new();

    if !path_fields.is_empty() {
        sub_structs.push(sub_struct_def(&path_struct, &path_fields));
        merge_params.push(quote! { __path: #path_struct });
        merge_fields.extend(path_fields.iter().map(|f| {
            let id = &f.ident;
            quote! { #id: __path.#id }
        }));
    }
    if !query_fields.is_empty() {
        sub_structs.push(sub_struct_def(&query_struct, &query_fields));
        merge_params.push(quote! { __query: #query_struct });
        merge_fields.extend(query_fields.iter().map(|f| {
            let id = &f.ident;
            quote! { #id: __query.#id }
        }));
    }
    if !body_fields.is_empty() {
        sub_structs.push(sub_struct_def(&body_struct, &body_fields));
        merge_params.push(quote! { __body: #body_struct });
        merge_fields.extend(body_fields.iter().map(|f| {
            let id = &f.ident;
            quote! { #id: __body.#id }
        }));
    }

    let merge_fn = quote! {
        #[doc(hidden)]
        #[allow(dead_code, non_snake_case)]
        fn __from_request_parts(#(#merge_params),*) -> Self {
            Self {
                #(#merge_fields),*
            }
        }
    };

    // Closure extraction params in axum order (non-body extractors first,
    // `Json` last since it consumes the request body).
    let mut extract_params: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut call_args: Vec<proc_macro2::TokenStream> = Vec::new();

    if !path_fields.is_empty() {
        extract_params.push(quote! {
            ::sevria_service_kit::http::__private::Path(__path): ::sevria_service_kit::http::__private::Path<#path_struct>
        });
        call_args.push(quote! { __path });
    }
    if !query_fields.is_empty() {
        extract_params.push(quote! {
            ::sevria_service_kit::http::__private::Query(__query): ::sevria_service_kit::http::__private::Query<#query_struct>
        });
        call_args.push(quote! { __query });
    }
    if !body_fields.is_empty() {
        extract_params.push(quote! {
            ::sevria_service_kit::http::__private::Json(__body): ::sevria_service_kit::http::__private::Json<#body_struct>
        });
        call_args.push(quote! { __body });
    }

    let body = handler_body(
        validates,
        quote! { #name::__from_request_parts(#(#call_args),*) },
        None,
    );

    let closure = quote! {{
        let handler = handler.clone();
        move |#(#extract_params),*| async move {
            #body
        }
    }};

    let register_route = quote! {
        #[doc(hidden)]
        #[allow(dead_code)]
        #vis fn __register_route<F, Fut, R>(
            router: &mut ::sevria_service_kit::http::Router,
            path: &str,
            method: ::sevria_service_kit::http::Method,
            handler: F,
        ) where
            F: Fn(#name) -> Fut + Clone + Send + Sync + 'static,
            Fut: ::std::future::Future<Output = R> + Send,
            R: ::sevria_service_kit::http::__private::IntoResponse + Send + 'static,
        {
            use ::sevria_service_kit::http::__private as __axum;
            let route = match method {
                ::sevria_service_kit::http::Method::GET => __axum::routing::get(#closure),
                ::sevria_service_kit::http::Method::POST => __axum::routing::post(#closure),
                ::sevria_service_kit::http::Method::PUT => __axum::routing::put(#closure),
                ::sevria_service_kit::http::Method::PATCH => __axum::routing::patch(#closure),
                ::sevria_service_kit::http::Method::DELETE => __axum::routing::delete(#closure),
                ::sevria_service_kit::http::Method::OPTIONS => __axum::routing::options(#closure),
                ::sevria_service_kit::http::Method::HEAD => __axum::routing::head(#closure),
                ::sevria_service_kit::http::Method::TRACE => __axum::routing::trace(#closure),
                ::sevria_service_kit::http::Method::CONNECT => __axum::routing::connect(#closure),
            };
            router.add_route(path, route);
        }
    };

    // Stateful variant: the handler takes a single `Context<Request, State>`
    // parameter. The injected state is cloned out of the router's registered
    // state and provided through axum's `State` extractor.
    let state_body = handler_body(
        validates,
        quote! { #name::__from_request_parts(#(#call_args),*) },
        Some(quote! { (*__axum_state).clone() }),
    );

    let state_closure = quote! {{
        let handler = handler.clone();
        move |::sevria_service_kit::http::__private::State(__axum_state): ::sevria_service_kit::http::__private::State<::std::sync::Arc<State>>, #(#extract_params),*| async move {
            #state_body
        }
    }};

    let register_route_with_state = quote! {
        #[doc(hidden)]
        #[allow(dead_code)]
        #vis fn __register_route_with_state<State, F, Fut, R>(
            router: &mut ::sevria_service_kit::http::Router,
            path: &str,
            method: ::sevria_service_kit::http::Method,
            handler: F,
        ) where
            F: Fn(::sevria_service_kit::http::Context<#name, State>) -> Fut + Clone + Send + Sync + 'static,
            Fut: ::std::future::Future<Output = R> + Send,
            R: ::sevria_service_kit::http::__private::IntoResponse + Send + 'static,
            State: Clone + Send + Sync + 'static,
        {
            use ::sevria_service_kit::http::__private as __axum;
            let __state = router.state_arc::<State>().expect(
                "endpoint handler uses `Context<_, State>` but no matching state was \
                 registered (call `Router::with_state(...)`)",
            );
            let route = match method {
                ::sevria_service_kit::http::Method::GET => __axum::routing::get(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::POST => __axum::routing::post(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::PUT => __axum::routing::put(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::PATCH => __axum::routing::patch(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::DELETE => __axum::routing::delete(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::OPTIONS => __axum::routing::options(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::HEAD => __axum::routing::head(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::TRACE => __axum::routing::trace(#state_closure).with_state(__state),
                ::sevria_service_kit::http::Method::CONNECT => __axum::routing::connect(#state_closure).with_state(__state),
            };
            router.add_route(path, route);
        }
    };

    quote! {
        #(#sub_structs)*

        impl #name {
            #merge_fn
            #register_route
            #register_route_with_state
        }
    }
}

/// A `Deserialize` struct holding the fields for one request source.
fn sub_struct_def(struct_ident: &Ident, fields: &[&FieldInfo]) -> proc_macro2::TokenStream {
    let field_defs = fields.iter().map(|f| {
        let ident = &f.ident;
        let ty = &f.ty;
        let serde_attrs = &f.serde_attrs;
        quote! { #(#serde_attrs)* pub #ident: #ty }
    });
    quote! {
        #[doc(hidden)]
        #[allow(dead_code, non_camel_case_types)]
        #[derive(::serde::Deserialize)]
        struct #struct_ident {
            #(#field_defs),*
        }
    }
}

// ---------------------------------------------------------------------------
// #[response] attribute macro
// ---------------------------------------------------------------------------

/// Attribute macro for declaring response types with OpenAPI metadata.
///
/// Transforms a type alias into a newtype struct implementing `Endpoint`.
/// The `status` accepts either a numeric literal (`422`) or a `Status::*`
/// constant (`Status::UNPROCESSABLE_ENTITY`), matching `#[endpoint]`.
///
/// # Success response
///
/// ```ignore
/// #[openapi::response(status = 200, description = "Email sent successfully")]
/// pub type SendEmailResponse = Response<Email, ()>;
/// ```
///
/// # Error response
///
/// ```ignore
/// #[openapi::response(
///     status = Status::NOT_FOUND,
///     description = "Resource not found",
///     error = (code = "NOT_FOUND", message = "Resource not found"),
/// )]
/// pub type NotFoundErrorResponse = ErrorResponse<()>;
/// ```
pub(crate) fn response(attr: TokenStream, item: TokenStream) -> TokenStream {
    let ResponseAttr {
        status,
        description,
        summary,
        error_code,
        error_message,
        error_details,
        include_meta,
    } = match syn::parse(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };

    let type_alias: syn::ItemType = match syn::parse(item) {
        Ok(t) => t,
        Err(e) => return e.to_compile_error().into(),
    };

    expand_response(
        &type_alias,
        status,
        &description,
        summary.as_deref(),
        error_code,
        error_message,
        &error_details,
        include_meta,
    )
}

struct ResponseAttr {
    status: u16,
    description: String,
    summary: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    error_details: Vec<(String, String)>,
    include_meta: bool,
}

impl Parse for ResponseAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut status: Option<u16> = None;
        let mut description: Option<String> = None;
        let mut summary: Option<String> = None;
        let mut error_code: Option<String> = None;
        let mut error_message: Option<String> = None;
        let mut error_details: Vec<(String, String)> = Vec::new();
        let mut include_meta = false;

        while !input.is_empty() {
            let key: Ident = input.parse()?;

            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;

                match key.to_string().as_str() {
                    "status" => {
                        // Accept either `status = 422` or `status = Status::UNPROCESSABLE_ENTITY`.
                        let val: syn::Expr = input.parse()?;
                        status = Some(status_from_expr(&val)?);
                    }
                    "description" => {
                        let val: LitStr = input.parse()?;
                        description = Some(val.value());
                    }
                    "summary" => {
                        let val: LitStr = input.parse()?;
                        summary = Some(val.value());
                    }
                    "error" => {
                        let content;
                        syn::parenthesized!(content in input);
                        while !content.is_empty() {
                            let ek: Ident = content.parse()?;
                            content.parse::<Token![=]>()?;
                            match ek.to_string().as_str() {
                                "code" => {
                                    let val: LitStr = content.parse()?;
                                    error_code = Some(val.value());
                                }
                                "message" => {
                                    let val: LitStr = content.parse()?;
                                    error_message = Some(val.value());
                                }
                                "details" => {
                                    let list_content;
                                    syn::parenthesized!(list_content in content);
                                    error_details = syn::punctuated::Punctuated::<
                                        ErrorDetailExample,
                                        Token![,],
                                    >::parse_terminated(
                                        &list_content
                                    )
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|e| (e.field, e.reason))
                                    .collect();
                                }
                                _ => {
                                    return Err(syn::Error::new(
                                        ek.span(),
                                        format!(
                                            "expected `code`, `message`, or `details`, found `{ek}`"
                                        ),
                                    ));
                                }
                            }
                            if !content.is_empty() {
                                content.parse::<Token![,]>()?;
                            }
                        }
                    }
                    _ => {
                        return Err(syn::Error::new(
                            key.span(),
                            format!("expected `status`, `description`, or `error`, found `{key}`"),
                        ));
                    }
                }
            } else {
                // Bare flag (no `= value`)
                match key.to_string().as_str() {
                    "meta" => include_meta = true,
                    _ => {
                        return Err(syn::Error::new(key.span(), format!("unknown flag `{key}`")));
                    }
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            status: status.ok_or_else(|| syn::Error::new(input.span(), "missing `status`"))?,
            description: description
                .ok_or_else(|| syn::Error::new(input.span(), "missing `description`"))?,
            summary,
            error_code,
            error_message,
            error_details,
            include_meta,
        })
    }
}

/// A single `(field = "...", reason = "...")` example inside `details`.
struct ErrorDetailExample {
    field: String,
    reason: String,
}

impl Parse for ErrorDetailExample {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        syn::parenthesized!(content in input);
        let mut field: Option<String> = None;
        let mut reason: Option<String> = None;
        while !content.is_empty() {
            let k: Ident = content.parse()?;
            content.parse::<Token![=]>()?;
            match k.to_string().as_str() {
                "field" => {
                    let val: LitStr = content.parse()?;
                    field = Some(val.value());
                }
                "reason" => {
                    let val: LitStr = content.parse()?;
                    reason = Some(val.value());
                }
                _ => {
                    return Err(syn::Error::new(
                        k.span(),
                        format!("expected `field` or `reason`, found `{k}`"),
                    ));
                }
            }
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            field: field.ok_or_else(|| {
                syn::Error::new(input.span(), "missing `field` in detail example")
            })?,
            reason: reason.ok_or_else(|| {
                syn::Error::new(input.span(), "missing `reason` in detail example")
            })?,
        })
    }
}

fn expand_response(
    type_alias: &syn::ItemType,
    status: u16,
    description: &str,
    summary: Option<&str>,
    error_code: Option<String>,
    error_message: Option<String>,
    error_details: &[(String, String)],
    include_meta: bool,
) -> TokenStream {
    let type_name = &type_alias.ident;
    let vis = &type_alias.vis;
    let inner_type = &type_alias.ty;

    let status_literal = status;
    let has_details = !error_details.is_empty();

    let is_error = is_path_named(inner_type, "ErrorResponse");
    // Success responses (`Response<D, M>` aliases) expose a `new()` constructor;
    // error responses (`ErrorResponse<M>`) are built via `From<Error>` instead.
    let is_success = is_path_named(inner_type, "Response");
    let attrs: Vec<&Attribute> = type_alias
        .attrs
        .iter()
        .filter(|a| !a.path().is_ident("response"))
        .collect();

    // Build error detail examples array expression
    let details_examples: Vec<proc_macro2::TokenStream> = error_details
        .iter()
        .map(|(f, r)| {
            quote! {
                ::sevria_service_kit::__private::serde_json::json!({"field": #f, "reason": #r})
            }
        })
        .collect();

    let schema_impl = if is_error {
        let meta_ty = extract_error_response_type_args(inner_type);
        let code_example = error_code.as_deref().unwrap_or("");
        let message_example = error_message.as_deref().unwrap_or("");

        // Error properties: code, message, optionally details
        let mut error_props = proc_macro2::TokenStream::new();
        error_props.extend(quote! {
            "code": { "type": "string", "example": #code_example },
            "message": { "type": "string", "example": #message_example },
        });
        if has_details {
            error_props.extend(quote! {
                "details": {
                    "type": "array",
                    "items": <::sevria_service_kit::ErrorDetail as ::sevria_service_kit::http::Endpoint>::json_schema(),
                    "example": [ #(#details_examples),* ]
                },
            });
        }

        // Error response — success is always false
        let mut props = proc_macro2::TokenStream::new();
        props.extend(quote! {
            props.insert("success".into(), ::sevria_service_kit::__private::serde_json::json!({"type": "boolean", "example": false}));
        });
        // Error field
        props.extend(quote! {
            props.insert("error".into(), ::sevria_service_kit::__private::serde_json::json!({
                "type": "object",
                "properties": { #error_props },
                "required": ["code", "message"]
            }));
        });
        // Meta field (only when requested and the meta type is not `()`)
        if include_meta && !is_unit_type(&meta_ty) {
            props.extend(quote! {
                props.insert("meta".into(), <#meta_ty as ::sevria_service_kit::http::Endpoint>::json_schema());
            });
        }

        quote! {
            impl ::sevria_service_kit::http::Endpoint for #type_name {
                fn description() -> &'static str {
                    #description
                }

                fn json_schema() -> ::sevria_service_kit::__private::serde_json::Value {
                    let mut props = ::sevria_service_kit::__private::serde_json::Map::new();
                    #props
                    ::sevria_service_kit::__private::serde_json::json!({
                        "type": "object",
                        "properties": props,
                        "required": ["success"]
                    })
                }
            }
        }
    } else {
        let (data_ty, meta_ty) = extract_response_type_args(inner_type);

        let mut props = proc_macro2::TokenStream::new();
        props.extend(quote! {
            props.insert("success".into(), ::sevria_service_kit::__private::serde_json::json!({"type": "boolean"}));
        });
        // `data` is omitted when the data type is `()` (it is always `None`).
        if !is_unit_type(&data_ty) {
            props.extend(quote! {
                props.insert("data".into(), <#data_ty as ::sevria_service_kit::http::Endpoint>::json_schema());
            });
        }
        // `meta` is omitted when not requested or when the meta type is `()`.
        if include_meta && !is_unit_type(&meta_ty) {
            props.extend(quote! {
                props.insert("meta".into(), <#meta_ty as ::sevria_service_kit::http::Endpoint>::json_schema());
            });
        }

        quote! {
            impl ::sevria_service_kit::http::Endpoint for #type_name {
                fn description() -> &'static str {
                    #description
                }

                fn json_schema() -> ::sevria_service_kit::__private::serde_json::Value {
                    let mut props = ::sevria_service_kit::__private::serde_json::Map::new();
                    #props
                    ::sevria_service_kit::__private::serde_json::json!({
                        "type": "object",
                        "properties": props,
                        "required": ["success"]
                    })
                }
            }
        }
    };

    // Hidden metadata: status, description, and optional summary used by the endpoint macro
    let summary_str = summary.unwrap_or("");
    let has_summary = summary.is_some();
    // Success responses get an ergonomic `new()` that yields `success: true`.
    let new_fn = if is_success {
        quote! {
            /// Construct an empty success response (`success: true`).
            pub fn new() -> Self {
                Self(<#inner_type>::new())
            }
        }
    } else {
        proc_macro2::TokenStream::new()
    };
    let meta_fn = quote! {
        impl #type_name {
            #new_fn

            #[doc(hidden)]
            pub const fn __openapi_response_meta() -> (u16, &'static str) {
                (#status_literal, #description)
            }

            #[doc(hidden)]
            pub const fn __openapi_response_summary() -> Option<&'static str> {
                if #has_summary {
                    Some(#summary_str)
                } else {
                    None
                }
            }
        }
    };

    let expanded = quote! {
        #(#attrs)*
        #[derive(::serde::Serialize)]
        #vis struct #type_name(pub #inner_type);

        #schema_impl

        #meta_fn
    };

    expanded.into()
}

/// Check if a type path's last segment matches the given name.
fn is_path_named(ty: &Type, name: &str) -> bool {
    if let Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|s| s.ident == name)
            .unwrap_or(false)
    } else {
        false
    }
}

/// Returns `true` when the syntax tree type is the unit type `()`.
fn is_unit_type(ty: &Type) -> bool {
    match ty {
        Type::Paren(inner) => is_unit_type(&inner.elem),
        Type::Tuple(tuple) => tuple.elems.is_empty(),
        _ => false,
    }
}

/// Extract the two generic args from `Response<Data, Meta>`.
fn extract_response_type_args(ty: &Type) -> (Type, Type) {
    if let Type::Path(type_path) = ty {
        if let Some(seg) = type_path.path.segments.last() {
            if let syn::PathArguments::AngleBracketed(ref args) = seg.arguments {
                let mut types = args.args.iter().filter_map(|a| {
                    if let syn::GenericArgument::Type(t) = a {
                        Some(t.clone())
                    } else {
                        None
                    }
                });
                let data = types.next().unwrap_or_else(|| syn::parse_quote! { () });
                let meta = types.next().unwrap_or_else(|| syn::parse_quote! { () });
                return (data, meta);
            }
        }
    }
    (syn::parse_quote! { () }, syn::parse_quote! { () })
}

/// Extract the single generic arg from `ErrorResponse<Meta>`.
fn extract_error_response_type_args(ty: &Type) -> Type {
    if let Type::Path(type_path) = ty {
        if let Some(seg) = type_path.path.segments.last() {
            if let syn::PathArguments::AngleBracketed(ref args) = seg.arguments {
                if let Some(syn::GenericArgument::Type(t)) = args.args.first() {
                    return t.clone();
                }
            }
        }
    }
    syn::parse_quote! { () }
}

/// Build a `serde_json::json!({ ... })` expression from the parsed fields.
fn build_json_schema(field_infos: &[FieldInfo]) -> proc_macro2::TokenStream {
    let mut field_builders: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut required_names: Vec<String> = Vec::new();

    for fi in field_infos {
        let json_name = &fi.json_name;
        let json_type = infer_json_type(&fi.ty);

        // Add description/example to the field schema when present.
        let mut schema_mutators: Vec<proc_macro2::TokenStream> = Vec::new();
        if let Some(d) = &fi.description {
            let d = d.as_str();
            schema_mutators.push(quote! {
                obj.insert("description".into(), ::sevria_service_kit::__private::serde_json::Value::String(#d.into()));
            });
        }
        if let Some(ex) = &fi.example {
            let ex = ex.as_str();
            schema_mutators.push(quote! {
                obj.insert("example".into(), ::sevria_service_kit::__private::serde_json::Value::String(#ex.into()));
            });
        }

        let field_builder = if schema_mutators.is_empty() {
            quote! {
                props.insert(#json_name.into(), #json_type);
            }
        } else {
            quote! {
                {
                    let mut schema = #json_type;
                    if let Some(obj) = schema.as_object_mut() {
                        #(#schema_mutators)*
                    }
                    props.insert(#json_name.into(), schema);
                }
            }
        };

        field_builders.push(field_builder);

        if fi.required {
            required_names.push(json_name.clone());
        }
    }

    let required_arr: Vec<proc_macro2::TokenStream> = required_names
        .iter()
        .map(|n| {
            let s = n.as_str();
            quote! { #s }
        })
        .collect();

    quote! {
        {
            let mut props = ::sevria_service_kit::__private::serde_json::Map::new();
            #(#field_builders)*
            ::sevria_service_kit::__private::serde_json::json!({
                "type": "object",
                "properties": props,
                "required": [#(#required_arr),*]
            })
        }
    }
}

/// Check `#[serde(rename = "...")]` on a field.
fn serde_rename(field: &syn::Field) -> Option<String> {
    for attr in &field.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }

        let mut rename: Option<String> = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let val: LitStr = meta.value()?.parse()?;
                rename = Some(val.value());
            }
            Ok(())
        });

        if let Some(r) = rename {
            return Some(r);
        }
    }
    None
}

/// Check if a type is `Option<T>`.
fn is_option_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(last) = type_path.path.segments.last() {
            return last.ident == "Option";
        }
    }
    false
}

/// Map a `syn::Type` to a JSON Schema type value token stream
/// (e.g. `{ "type": "string" }`).
fn infer_json_type(ty: &syn::Type) -> proc_macro2::TokenStream {
    // Unwrap `Option<T>` to get the inner type
    let inner_ty = if is_option_type(ty) {
        if let syn::Type::Path(type_path) = ty {
            if let syn::PathArguments::AngleBracketed(args) =
                &type_path.path.segments.last().unwrap().arguments
            {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    inner
                } else {
                    ty
                }
            } else {
                ty
            }
        } else {
            ty
        }
    } else {
        ty
    };

    if let syn::Type::Path(type_path) = inner_ty {
        if let Some(last) = type_path.path.segments.last() {
            let name = last.ident.to_string();
            return match name.as_str() {
                "String" | "str" => {
                    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "string" }) }
                }
                "bool" => {
                    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "boolean" }) }
                }
                "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
                | "u128" | "usize" => {
                    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "integer" }) }
                }
                "f32" | "f64" => {
                    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "number" }) }
                }
                "Vec" => {
                    // For Vec<T>, generate { "type": "array", "items": <T schema> }
                    if let syn::PathArguments::AngleBracketed(args) = &last.arguments {
                        if let Some(syn::GenericArgument::Type(item_ty)) = args.args.first() {
                            let item_schema = infer_json_type(item_ty);
                            return quote! {
                                ::sevria_service_kit::__private::serde_json::json!({
                                    "type": "array",
                                    "items": #item_schema
                                })
                            };
                        }
                    }
                    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "array" }) }
                }
                _ => {
                    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "string" }) }
                }
            };
        }
    }

    quote! { ::sevria_service_kit::__private::serde_json::json!({ "type": "string" }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_struct(src: &str) -> syn::ItemStruct {
        syn::parse_str(src).expect("parse struct")
    }

    #[test]
    fn detects_garde_on_fields() {
        let s = parse_struct(
            "struct Email {\n    #[garde(email)]\n    from: String,\n    to: String,\n}",
        );
        assert!(derives_validate(&s));
    }

    #[test]
    fn detects_garde_on_struct_and_schema_validate() {
        assert!(derives_validate(&parse_struct(
            "#[garde(custom(validate))]\nstruct Probe {\n    field: String,\n}"
        )));
        assert!(derives_validate(&parse_struct(
            "#[schema(validate)]\nstruct Probe {\n    field: String,\n}"
        )));
    }

    #[test]
    fn does_not_detect_without_garde() {
        let s = parse_struct("struct Probe {\n    field: String,\n}");
        assert!(!derives_validate(&s));
    }
}
