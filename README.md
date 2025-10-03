# matrix-user-swap

A tool to migrate between matrix user accounts. This is the same general idea as
[EMS Migrator], but with the intent of being less bad.

[EMS Migrator]: https://ems.element.io/tools/matrix-migration

## Migrated State

 - Joined rooms
 - User power level in each joined room
 - Tags in each joined room (the `m.tag` room account data event)
 - Direct message mappings (the `m.direct` global account data event)
 - Ignored users (the `m.ignored_user_list` global account data event)

## Limitations

 - Rate-limiting is not implemented. This will likely only work with conduit-family servers, which don't implement server-side rate limiting. (planned)
 - Migrating between users on different homeservers is unsupported. (planned)

State that is not migrated

 - Global displayname/avatar and per-room displaynames/avatars (planned)
 - [MSC2545] user custom emotes/sticker packs (planned)
 - Invited/left rooms (not planned, infeasible)
 - Knocked rooms (not planned)
 - Room history (not planned, infeasible)
 - Push rules and client settings (not planned)

[MSC2545]: https://github.com/matrix-org/matrix-spec-proposals/pull/2545

## Installation

### With nix

```bash
git clone https://gitlab.computer.surgery/olivia/matrix-user-swap.git
cd matrix-user-swap
nix-build
```

The binary will be in `result/bin/matrix-user-swap`.

### Without nix

Install rust with rustup (or your system package manager), then:

```bash
git clone https://gitlab.computer.surgery/olivia/matrix-user-swap.git
cd matrix-user-swap
cargo build --release
```

The binary will be in `target/release/matrix-user-swap`.

## Usage

Acquire access tokens from the old and new accounts. This can generally be
extracted from a logged-in client. In Element, the access token is under the
"Help & About" tab in settings.
