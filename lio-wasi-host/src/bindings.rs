//! Bindings for the `wasi:cli/command` world of WASI 0.3, generated from the
//! WIT in `wit/`.

wasmtime::component::bindgen!({
    path: "wit",
    world: "wasi:cli/command",
    imports: {
        "wasi:cli/stdin": store | trappable,
        "wasi:cli/stdout": store | trappable,
        "wasi:cli/stderr": store | trappable,
        "wasi:filesystem/types.[method]descriptor.read-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.write-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.append-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.read-directory": store | trappable,
        "wasi:sockets/types.[method]tcp-socket.listen": store | trappable,
        "wasi:sockets/types.[method]tcp-socket.send": store | trappable,
        "wasi:sockets/types.[method]tcp-socket.receive": store | trappable,
        default: trappable,
    },
    exports: { default: async | store },
    with: {
        "wasi:cli/terminal-input.terminal-input": crate::cli::TerminalInput,
        "wasi:cli/terminal-output.terminal-output": crate::cli::TerminalOutput,
        "wasi:filesystem/types.descriptor": crate::filesystem::Descriptor,
        "wasi:sockets/types.tcp-socket": crate::sockets::TcpSocket,
        "wasi:sockets/types.udp-socket": crate::sockets::UdpSocket,
    },
});
