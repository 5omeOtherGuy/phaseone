//! The `p1:channel/plugin` world's bindings, in their own crate for the reason
//! `modules/p1-bindings-tool` gives: the `export!` macro needs `unsafe`, which the component
//! crates forbid.
#![forbid(unsafe_code)]

pub mod generated {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        pub_export_macro: true,
        default_bindings_module: "p1_bindings_channel::generated",
    });
}
