# strata-ipc

Everything on the named pipe between the unelevated app and the elevated
[`strata-helper`](../strata-helper). It holds no scan logic; the helper wires these pieces to
the scanners. The pipe carries 3.2 million scan records per second.

## Responsibilities

- `protocol`: versioned messages (`Envelope`, `Request`, `Response`, handshake `Hello` /
  `Welcome`, streamed scan and file-activity events), each tagged with a request id. The
  current version is 3; the handshake accepts only an exact version match, and new variants
  and fields are only ever appended.
- `frame`: length-prefixed binary framing with postcard payloads (`encode_frame`,
  `decode_frame`), strictly bounds-checked against hostile input.
- `pipe`: overlapped transport: `PipeServer` for the helper, `PipeClient` for the app, with
  timeouts and typed disconnects.
- `security`: the pipe DACL (current user + SYSTEM) and integrity label, unguessable session
  pipe names (`session_pipe_name`), and peer verification by image path and Authenticode
  signer before any message is parsed.
- `rate`: per-connection token bucket.

## Test

```powershell
cargo test -p strata-ipc
cargo bench -p strata-ipc --bench ipc
```

The pipe tests run end to end on real named pipes: handshake, version mismatch, untrusted
peers, malformed and oversized frames, disconnects mid-stream, rate limiting.

## See also

- Reporting a vulnerability: [SECURITY.md](../../SECURITY.md)
