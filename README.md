# ampc-common
ampc-common repository provides Rust utilities that can be reused by any AMPC setup.

## Releases

The [release workflow](.github/workflows/release.yml) uses release-plz to prepare
version and changelog pull requests on pushes to `main`. It can also be run
manually from `main`. Merging a release pull request creates GitHub releases and
tags named `<crate>-v<version>`, for example `ampc-server-utils-v0.1.0`.
All workspace members are managed independently; no crates are published to a
Cargo registry. Key Manager container images use the separate
[Build and Publish Key Manager workflow](.github/workflows/key-manager-build-and-push.yml).

In the repository's **Settings > Actions > General > Workflow permissions**, enable
**Allow GitHub Actions to create and approve pull requests**. The workflow uses
the built-in `GITHUB_TOKEN`; no registry credentials are needed.

CI does not run automatically on release pull requests created with `GITHUB_TOKEN`.
Before merging, push a commit to the release branch using a maintainer account
to trigger the checks. An empty commit is sufficient.

## License

Unless otherwise specified, all code in this repository is dual-licensed under
either:

- MIT License ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0, with LLVM Exceptions ([LICENSE-APACHE](LICENSE-APACHE))

at your option. This means you may select the license you prefer to use.

Any contribution intentionally submitted for inclusion in the work by you, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
