//! Procedural macros for the Sevria service stack.
//!
//! Proc-macro crates require all `#[proc_macro]`, `#[proc_macro_derive]`, and
//! `#[proc_macro_attribute]` functions to live at the crate root. The heavy
//! lifting lives in the [`openapi`] module; the functions here are thin
//! wrappers that re-export the public macro surface.

mod openapi;

use proc_macro::TokenStream;

/// Attribute macro that annotates an async handler function with OpenAPI
/// metadata and generates a hidden route-handle const.
#[proc_macro_attribute]
pub fn endpoint(attr: TokenStream, item: TokenStream) -> TokenStream {
    openapi::endpoint(attr, item)
}

/// Attribute macro for an inherent `impl` block whose methods are annotated
/// with `#[endpoint(...)]`. Generates a `pub fn into_router(self) -> Router`
/// that registers every endpoint route plus its OpenAPI metadata.
#[proc_macro_attribute]
pub fn router(attr: TokenStream, item: TokenStream) -> TokenStream {
    openapi::router(attr, item)
}

/// Derive macro that implements `Endpoint` for a type.
///
/// Reads the `#[openapi(description = "...")]` attribute on the struct.
#[proc_macro_derive(Schema, attributes(openapi))]
pub fn schema(item: TokenStream) -> TokenStream {
    openapi::derive_schema(item)
}

/// Attribute macro for declaring response types with OpenAPI metadata.
#[proc_macro_attribute]
pub fn response(attr: TokenStream, item: TokenStream) -> TokenStream {
    openapi::response(attr, item)
}
