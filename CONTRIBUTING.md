# Contributing

Thanks for taking an interest. Bug reports, fixes, tests and documentation improvements are welcome. For a larger
change, please open an issue first so the approach can be agreed before you spend time on it.

## Building and testing

The Rust toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use. A change must pass the same
checks CI runs, with warnings denied:

```
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo doc --locked --workspace --no-deps
cargo deny check                  # dependency policy (needs cargo-deny)
```

If you touch `crates/teams-client`, also check each backend on its own:

```
cargo clippy -p teams-client --all-targets --no-default-features
cargo clippy -p teams-client --all-targets --no-default-features --features graph
cargo clippy -p teams-client --all-targets --no-default-features --features teams
```

One test runs the script that decrypts the Teams sign-in cache under [Node](https://nodejs.org) (the same WebCrypto a
browser has), against known answers made with Node's own `crypto` module. Without Node installed it is skipped on your
machine; CI always has Node and fails if it is missing.

The syntax highlighter is a git dependency (`hkimura-intersys/syntax-color-objectscript`), so the first build needs to
reach GitHub; `Cargo.lock` pins the exact commit.

You can try the application without an account: `cargo run --release -- --demo`. `--demo --dump` renders a frame as
text, which is handy for checking layout (see the README).

### Running the Unix-only tests from Windows

Some tests exist only on Unix (file and folder permissions, for example). They are not compiled on Windows, so a
Windows machine never runs them; CI does, on Linux and macOS. To run the library's from Windows, build the tests for
Linux and run the result in WSL (the WSL distribution needs neither Rust nor a C compiler):

```
rustup target add x86_64-unknown-linux-musl
$sysroot = rustc --print sysroot
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "$sysroot\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe"
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C linker-flavor=ld.lld"
cargo test -p teams-client --no-default-features --target x86_64-unknown-linux-musl --no-run
```

The last command prints the path of the test program on a line that starts "Executable unittests". Copy that file into
WSL, because a program on the Windows drive is not executable there, and run it:

```
wsl cp /mnt/c/<path to the test program> ~/teams_client_tests
wsl chmod +x ~/teams_client_tests
wsl ~/teams_client_tests
```

This covers only the library without its backends (`--no-default-features`). The backends need a C compiler for the
target (for `ring`), and so does the application (for the syntax grammars), so their tests run in CI only.

## How the code is organised

`AGENTS.md` has the code map, the conventions, the design decisions, and what is still unverified. The short version:

- `crates/teams-client` is the library that talks to Microsoft. It takes no settings from the environment, picks no
  path and never prints (browser discovery for the sign-in consults `PATH` and the standard install folders). The
  application (`src/`) does that.
- The `Backend` trait is the seam between the application and a service. Graph, the Teams web API and the demo all
  implement it.

## The unofficial Teams web mode

`--teams` (the `teams` backend) is unofficial and unsanctioned, and the project says so where it mentions it (the
README's disclaimer, `--help`, the settings file's documentation, `SECURITY.md` and the crate documentation). Keep that
wording when you change those places. Features that exist to hide the client from Microsoft or an administrator, or to
get around a tenant's block, will not be accepted. Do not copy code or data from AGPL-licensed projects.

## Conventions

- Default `rustfmt`. No `unsafe` (it is forbidden by lint), and no `unwrap` outside tests.
- Library errors are typed (`teams_client::Error` and its layered variants), never strings. Turning an error into text
  happens only at the UI boundary.
- Text that comes from the network is passed through `teams_client::clean` (or `html_to_plain`) before it is shown.
- Credentials go only to their own service. Do not add a request that sends a token to a URL taken from a response
  without checking it, and do not follow redirects with credentials attached.
- Anything that can run on the UI thread against remote content needs a bound (size, depth or count).
- Tests are deterministic. Application tests build the app with `App::for_test`, which runs background work on the
  calling thread and uses an in-memory clipboard and a recording browser. Library tests that need HTTP use the scripted
  loopback server in `testserver.rs`. Nothing in a test may touch the real network, clipboard, browser or settings
  directory.
- Add a test with a fix. For parsers and anything fed by remote data, a property test is welcome.
- Comments and documentation describe what the code does and why it is that way. They do not tell the history of how it
  got there.

## Reporting a problem with a service

If vtteams misreads what Teams or Graph sends, run it with `--record-shapes DIR` and attach the files. They contain the
structure of each request and response with all content masked, so they are safe to share. `vtteams.log` (see "Logging" in
the README) also helps: it records which fields vtteams had not seen before and the masked structure of anything it could
not read. It holds no message text, but read it through before attaching it. Please do not paste tokens, message text
or other people's names into an issue.

For a security problem, follow `SECURITY.md` instead of opening a public issue.

## Pull requests

- Keep a change focused, and explain what it does and why.
- Make sure the checks above pass.
- Update `README.md`, `AGENTS.md` or the crate README if behaviour or conventions change.

## License

By contributing you agree that your contribution is licensed under the MIT License, the same as the project (see
`LICENSE`).

