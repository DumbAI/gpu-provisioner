# Releasing

`gpuprov` ships as the `gpu-provisioner` crate on crates.io; it provides the
`gpuprov` binary and the `gpu_provisioner` library.

## Checklist

1. Bump `version` in `Cargo.toml` (semver).
2. Make sure CI is green, and run the e2e suite locally if Docker-touching code changed.
3. Check what will be uploaded. `Cargo.toml` `include` limits the package to
   `src/`, `tests/`, `examples/*.yaml`, `Cargo.lock`, the README, and the
   LICENSE. The docs site and design notes stay out.

    ```bash
    cargo package --list
    cargo publish --dry-run
    ```

4. Publish (irreversible: a version can be yanked but never re-used or deleted):

    ```bash
    cargo login            # token from https://crates.io/me
    cargo publish
    git tag v0.1.0 && git push origin v0.1.0
    ```

## Docs site

The site is built with [MkDocs](https://www.mkdocs.org) and the Material theme
from `docs/`:

```bash
uv run --with-requirements docs/requirements.txt mkdocs serve        # http://127.0.0.1:8000
uv run --with-requirements docs/requirements.txt mkdocs build --strict
uv run --with-requirements docs/requirements.txt mkdocs gh-deploy    # publish to GitHub Pages
```

(Use `pip install -r docs/requirements.txt` in place of `uv` if you prefer.)

The live site is <https://dumbai.github.io/gpu-provisioner/>, served by GitHub
Pages from the `gh-pages` branch. `.github/workflows/docs.yml` builds the site
strictly on pull requests and redeploys it on every push to `main` that touches
`docs/` or `mkdocs.yml`, so `mkdocs gh-deploy` by hand is only needed out of band.
