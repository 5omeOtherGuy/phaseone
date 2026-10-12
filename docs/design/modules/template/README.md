# A p1 module, built outside phaseone

This directory is a template module package: copy it into a repository of your own, rename it, and
build it with the published guest SDK. The author guide is
[`../README.md`](../README.md#writing-a-module-outside-this-repository); the decisions behind it
are ADR-0209 (external modules) and ADR-0210 (the SDK and its versions).

| File | What it is |
|---|---|
| `Cargo.toml` | the crate, its `p1-module-sdk` git dependency pinned by tag, and the package manifest under `[package.metadata.p1-module]` |
| `src/lib.rs` | `echo`, a tool that returns its `text` argument |
| `build.sh` | builds the component with the pinned toolchain and packs `target/p1-module/<crate>.p1pkg` |
| `conformance.sh` | runs p1's author conformance checks on the built archive |
| `rust-toolchain.toml` | the Rust and the wasm target p1 builds with |
| `.github/workflows/module.yml` | CI: build, conformance, and a release asset per `v*` tag |

```sh
./build.sh                                   # target/p1-module/acme-module-echo.p1pkg
./conformance.sh                             # p1's checks, against that archive
p1 modules install target/p1-module/acme-module-echo.p1pkg
```

`p1 modules install` pins the package's digest in your `modules.lock` under the module key its name
declares (`echo` for `acme/echo`); an environment then names it in `[[tools]] module = "echo"`.

Before publishing, rename the crate (`[package] name`) and the package (`[package.metadata.p1-module]
name = "<you>/<module>"`): the namespace is yours, `p1/` is reserved for p1's release.
