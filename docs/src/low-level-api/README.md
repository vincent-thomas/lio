# The Core Model

The low-level API is the heart of `lio`.

Even when you use a convenience wrapper like `TcpListener`, you are still using the same underlying design:

- an operation type describes work
- `Lio` owns the execution of that work
- completion is translated back into typed Rust data

This section explains the model in the order most readers need it:

1. how operations are driven
2. how resources and buffers are owned
3. how completions are delivered
4. how the lower-level `OpModel` machinery is shaped underneath that surface

That order matters. If you start with internal machinery before understanding explicit progress, ownership, and completion flow, `lio` looks harder than it is.

## Why the low-level API matters

The current source tree makes one thing clear: the low-level API is not a fallback layer. It is the main public abstraction.

That is where you find control over:

- files and directories
- sockets
- timers
- process creation
- buffer-based byte transfer

The higher-level modules are intentionally thinner and build on top of this layer.

That point is easy to miss if you come to the repository expecting the interesting part to be higher-level wrappers. In `lio`, the low-level layer is the thing to understand first because it already contains the real execution model.

## The shape of an operation

Most user-facing work starts with a constructor in `lio::api`, such as:

- `api::read`
- `api::write`
- `api::openat`
- `api::connect`
- `api::sleep`

Each constructor returns an `Io<T>` or `IoStream<T>`.

That immediately tells you three things:

- the operation has a type
- its result shape is known
- it has not necessarily been consumed yet

The library is careful about typed completion. A read does not complete as an unstructured event. It completes as a value whose shape is tied to the operation you started.

That is the core move the crate makes over raw backend events: it preserves the OS-facing shape of the operation while turning completion back into typed Rust data.

## Why this layer feels syscall-oriented

The source code closely mirrors the operating system:

- path-relative filesystem calls use directory resources and `CString` paths
- networking starts with socket creation and then bind/listen/connect/accept
- read and write operations own their buffers

That is deliberate. `lio` is not trying to invent a new storage or networking model. It is trying to make the existing one composable and asynchronous without losing control over the underlying mechanics.

## What this buys you

Using a syscall-shaped API gives `lio` three properties that matter when you build real programs on top of it.

### It stays honest about capability

You can usually tell what the library does by reading the operation name. There is very little magic.

### It keeps cost visible

Because the model stays close to the OS, resource ownership, buffer lifetime, and completion handling remain visible in user code.

### It supports thin higher-level wrappers

The `net` layer works because the lower layer is already expressive enough. `TcpSocket` is useful precisely because it does not need a hidden runtime or a second execution model.

Keep one sentence in mind as you read the rest of this section: an operation is a typed description of work, and `Lio` is the thing that drives it.

The next chapters build outward from that sentence. First they explain how an operation becomes active and makes progress. Then they explain ownership and completion. Only after that do they drop down to `OpModel`.
