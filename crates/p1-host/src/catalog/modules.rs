//! The WebAssembly package loader's catalog entry point: the package loader
//! (ADR-0071) registers the modules it loaded here, next to the compiled providers
//! and tools. Slice S1.4 fills it; until then no module is loaded, so it holds no code.
