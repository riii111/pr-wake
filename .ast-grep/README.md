# ast-grep

ast-grep checks repository source conventions. Rules live in `.ast-grep/rules/`; valid and invalid examples live in `.ast-grep/tests/`.

From the repository root, with the ast-grep version pinned in `.github/workflows/ci.yml`:

```sh
ast-grep test --skip-snapshot-tests
ast-grep scan
lefthook install # Enable the pre-commit hook (once).
```

The hook scans source files; CI runs both rule tests and the scan.

Checks use source syntax; they do not expand macros or resolve modules across files.
