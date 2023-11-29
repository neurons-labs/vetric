//! Procedural macros for the `vetric` metrics client.
//!
//! All macros in this crate are re-exported from the [`vetric`] crate. See its docs for more
//! details and the examples of usage.

// General settings.
#![recursion_limit = "128"]

extern crate proc_macro;

use proc_macro::TokenStream;

mod labels;
mod metrics;
mod register;
mod utils;

#[proc_macro_derive(Metrics, attributes(metrics))]
pub fn metrics(input: TokenStream) -> TokenStream {
    metrics::impl_metrics(input)
}

#[proc_macro_derive(EncodeLabelValue, attributes(metrics))]
pub fn encode_label_value(input: TokenStream) -> TokenStream {
    labels::impl_encode_label_value(input)
}

#[proc_macro_derive(EncodeLabelSet, attributes(metrics))]
pub fn encode_label_set(input: TokenStream) -> TokenStream {
    labels::impl_encode_label_set(input)
}

#[proc_macro_attribute]
pub fn register(_attrs: TokenStream, input: TokenStream) -> TokenStream {
    register::impl_register(input)
}
