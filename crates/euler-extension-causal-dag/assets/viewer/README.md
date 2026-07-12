# Causal DAG viewer assets

The four HTML templates and `runtime.js` were extracted from the previously
committed `docs/examples/knuth-gpt55-xhigh.html` design bundle. The runtime is
locally hardened for generated exports: sibling-component loading, external
module loading, and CDN fallbacks are disabled.

`react.production.min.js` and `react-dom.production.min.js` are the pinned
React 18.3.1 UMD production builds. Their upstream SHA-384 digests are:

```text
react      DGyLxAyjq0f9SPpVevD6IgztCFlnMF6oW/XQGmfe+IsZ8TqEiDrcHkMLKI6fiB/Z
react-dom  gTGxhz21lVGYNMcdJOyq01Edg0jhn/c22nsx0kyqP0TxaV5WVdsSH1fSDUf5YJj1
```

The ReactDOM diagnostic hyperlink was replaced with a local URN so generated
HTML contains no external application host. Namespace identifiers such as
`http://www.w3.org/2000/svg` are data identifiers, not network resources.
See `LICENSE.react.txt` for the vendored runtime license.

The inherited runtime uses `new Function` to compile the crate-owned
`text/dc-logic` blocks, so generated files explicitly allow `unsafe-eval` in
their CSP. Injected graph data is script-escaped JSON and is never evaluated as
code. The same CSP sets `connect-src 'none'`, and all external/sibling loaders
have been removed from the runtime.

`runtime.js` is intentionally kept as one formatted 1,500-line vendored asset:
it is the shared runtime for all four owner-supplied templates, and retaining a
single auditable copy avoids four parallel implementations. Viewer behavior
specific to Euler remains in the smaller HTML templates.
