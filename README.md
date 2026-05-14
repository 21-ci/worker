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
# port 3000 = path-routed API + invocation, port 3030 = domain-routed dispatch
docker run -p 3000:3000 -p 3030:3030 -v "./wasm_files:/data/wasm_files" \
  ghcr.io/21-ci/worker:distroless-latest
```

### Configuration (environments)

| Var | Default                                                     | Meaning |
|-----|-------------------------------------------------------------|---------|
| `BIND_ADDR` | `0.0.0.0:3000`                                              | host:port for the path-routed API (`/init`, `/{id}`) |
| `DOMAIN_BIND_ADDR` | `0.0.0.0:3030`                                              | host:port for the domain-routed listener (dispatches by `Host` header + path prefix) |
| `WASM_FILES_DIR` | `<crate>/wasm_files` (source) / `/data/wasm_files` (Docker) | where enrolled `.wasm` files live |
| `AUTH_TOKEN` | unset -> no auth                                            | bearer token required for `POST /init` |
| `POOL_INSTANCES` | `8192`                                                      | wasmtime pooling-allocator slot count; `0` switches to OnDemand |
| `WASM_LOGS` | `0`                                                         | when `1`, inherit guest stdout/stderr to the host |
| `STATS_LOG` | `0`                                                         | when `1`, print one `[stats]` line per request to stderr |

You can pass through these settings while running the instance with `-e KEY=VALUE`, 
like below:

```sh
docker run --rm -p 3000:3000 \
  -v "./wasm_files:/data/wasm_fileZs" \
  -e AUTH_TOKEN=$(openssl rand -hex 32) \
  -e STATS_LOG=1 \
  ghcr.io/21-ci/worker:distroless-latest
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

### Enrolling with a domain

In addition to the path-routed API on `BIND_ADDR`, worker runs a second listener
on `DOMAIN_BIND_ADDR` (default `:3030`) that dispatches by the request's `Host`
header and path prefix. Add `?domain=<host>[/<base/path>]` on enrollment to mount
a component under that domain + base path.

```sh
# mount UUID-1 at test.com root (serves test.com/*)
curl -X POST --data-binary @root.wasm \
  "http://localhost:3000/init?name=UUID-1&domain=test.com"
# -> UUID-1

# mount UUID-2 at test.com/somefuncs (serves test.com/somefuncs/*)
curl -X POST --data-binary @sub.wasm \
  "http://localhost:3000/init?name=UUID-2&domain=test.com/somefuncs"
# -> UUID-2
```

The domain listener picks the **longest matching base path**, so a request to
`test.com/somefuncs/foo` hits UUID-2 (inner path = `/foo`), while `test.com/foo`
falls back to UUID-1.

```sh
# hits UUID-1 (test.com root)
curl -H "Host: test.com" http://localhost:3030/foo

# hits UUID-2 (test.com/somefuncs/*)
curl -H "Host: test.com" http://localhost:3030/somefuncs/foo
```

The domain mapping is persisted as a sidecar `${WASM_FILES_DIR}/<name>.domain`
file and reloaded on startup. The pair `(host, base_path)` must be unique;
re-enrolling the same `host+base` returns `409 Conflict`.


## Contributing

We'd love for you to create pull requests for this project from the development
branch (or use feat/X for separate functionality, that can be useful but not prod
ready or tested).
Releases are created from the main branch.
