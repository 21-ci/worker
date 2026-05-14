![](misc/images/banner.png)


[![License: GPL-3.0](https://img.shields.io/badge/License-GPL%203.0-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Rust](https://img.shields.io/badge/lang-Rust-blue.svg)](https://rust-lang.org)
![Unsecure for production](https://img.shields.io/badge/not%20production--ready-red.svg)

> Simple runner for WASM workloads

Worker is a service, for running both locally and in the cloud, which
utilizes [Web Assembly](https://webassembly.org) and
[Web Assembly System Interface](https://wasi.dev/interfaces) to run applications -
like those made with [YASWS Framework](https://github.com/21-ci/yasws).

We also plan on adding other connectors like *SQL, KV (key-value) and S3
file storages.*

## How?

### Running an instance

**worker** is written on Rust, and is also packaged to Docker image (which can be 
downloaded from ghcr.io), here is an example on how to run it locally:

```sh
# run unauthenticated, components stored in ./wasm_files on the host
docker run -p 3000:3000 ghcr.io/21ci/worker:latest -v "./wasm_files:/data/wasm_files"
```

### Configuration (environments)

| Var | Default                                                     | Meaning |
|-----|-------------------------------------------------------------|---------|
| `BIND_ADDR` | `0.0.0.0:3000`                                              | host:port to listen on |
| `WASM_FILES_DIR` | `<crate>/wasm_files` (source) / `/data/wasm_files` (Docker) | where enrolled `.wasm` files live |
| `AUTH_TOKEN` | unset -> no auth                                            | bearer token required for `POST /init` |
| `POOL_INSTANCES` | `8192`                                                      | wasmtime pooling-allocator slot count; `0` switches to OnDemand |
| `WASM_LOGS` | `0`                                                         | when `1`, inherit guest stdout/stderr to the host |
| `STATS_LOG` | `0`                                                         | when `1`, print one `[stats]` line per request to stderr |

You can pass through these settings while running the instance with `-e KEY=VALUE`, 
like below:

```sh
docker run --rm -p 3000:3000 \
  -v "./wasm_files:/data/wasm_files" \
  -e AUTH_TOKEN=$(openssl rand -hex 32) \
  -e STATS_LOG=1 \
  ghcr.io/21ci/worker:latest
```

### Enrolling a component

The `POST /init` endpoint accepts a raw `.wasm` body and returns the id you'll
use to invoke it.

```sh
# generate-a-uuid id (no name given)
curl -X POST --data-binary @app.wasm \
  http://localhost:3000/init
# -> 18c6cd8d-d00d-44ce-a910-3579fbe21e82 (random UUID)

# pick your own name (must be unique; [a-zA-Z0-9_-], 1-128 chars, not 'init')
curl -X POST --data-binary @app.wasm \
  "http://localhost:3000/init?name=NAME"
# -> NAME

# with auth (if AUTH_TOKEN is set)
curl -X POST --data-binary @app.wasm \
  -H "Authorization: Bearer $AUTH_TOKEN" \
  "http://localhost:3000/init?name=NAME"
# -> NAME
```

Enrolled components are written to `${WASM_FILES_DIR}/<name>.wasm` so they survive
restarts and are lazy-reloaded on first request after a restart.

Then, this instance can be invoked by `curl http://localhost:3000/{NAME}` (where NAME is 
either UUID or set name, both are received on the enrollment)


## Contributing

We'd love for you to create pull requests for this project from the development
branch (or use feat/X for separate functionality, that can be useful but not prod
ready or tested).
Releases are created from the main branch.
