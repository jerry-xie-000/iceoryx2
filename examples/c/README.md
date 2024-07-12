# Instructions

## Build

In the repository root folder, execute this steps.

```bash
cmake -S . -B target/ffi/build -DBUILD_EXAMPLES=ON
cmake --build target/ffi/build
```

## Run Examples

### Publish-Subscribe

Run in two separate terminals. Note, currently the examples run for 10 seconds.

```bash
target/ffi/build/examples/c/publish_subscribe/example_c_publisher
```

```bash
target/ffi/build/examples/c/publish_subscribe/example_c_subscriber
```

### Discovery

```bash
target/ffi/build/examples/c/discovery/example_c_discovery
```