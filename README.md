# matrix-discovery

A minimal library to resolve matrix C2S server names. This is primarily intended
for writing simple bot clients and matrix tools. Many of these can get away with
using [ruma] directly, and don't need the complexity of [matrix-rust-sdk]. Often
server discovery is the only thing you need that isn't handled by ruma.

[matrix-rust-sdk]: https://github.com/matrix-org/matrix-rust-sdk
