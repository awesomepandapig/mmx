# Running MMX

For a complete installation, use [Local Docker](docs/src/pages/installation/local.mdx),
[Helm](docs/src/pages/installation/helm.mdx), or [local Kubernetes](docs/src/pages/installation/kind.mdx).
Each includes the operator console. Then follow [Getting started](docs/src/pages/getting-started.mdx).

## Build from source with Docker

Host requirements: Rust and Cargo (tested with 1.99), a C compiler, Node 24/npm, and Docker Compose.

```sh
npm --prefix console ci
npm --prefix console run build
cargo build --release -p mmx
export PATH="$PWD/target/release:$PATH"
export MMX_COMPOSE_FILE="$PWD/deploy/compose.yaml"
mmx local up --build
mmx local status
mmx local credentials
```

The container build installs the services' native dependencies. Operator settings stay under
`deploy/config` and `deploy/.env`; Docker volumes hold exchange data. `mmx local down` preserves them.

## Native service development

To compile every Rust service directly on the host, install:

| Dependency | Requirement |
| --- | --- |
| Rust | 1.99.1, including rustfmt and Clippy |
| CMake | 3.30 or newer |
| Clang/libclang | C/C++ compilation and bindgen |
| Java | JDK 21 or newer, for Aeron/Gradle |
| Aeron | 1.54.0 media driver, matching rusteron-client |
| Node | 24 LTS, for the embedded console |

```sh
npm --prefix console ci
npm --prefix console run build
cargo build --workspace --release --locked
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-features --all-targets --locked
cargo fmt --all -- --check
```

`tools/` contains optional load generators, invariant checks, profiling, and local investigation
helpers. They are developer tools; installations and production operations use the native `mmx` CLI.
Run disruptive tests against disposable state. Benchmarks are documented under `tools/bench/`.

## Website and docs

The public sites run independently of exchange services:

```sh
npm --prefix web ci
npm --prefix web run dev
# Separate terminal:
npm --prefix docs ci
npm --prefix docs run dev
```

For production builds and Vercel configuration, see [deploy/vercel.md](deploy/vercel.md).
Browser validation lives in `tools/browser/`; CI captures mobile and desktop screenshots.
