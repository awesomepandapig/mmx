# MMX web

Remix landing page and browser-only exchange simulation. The `/demo` route uses local sample state;
it does not connect to Rust services or provide evidence of backend performance or durability.

```sh
npm ci
npm run dev
npm run typecheck
npm run lint
npm run build
npm start
```

Set `VITE_DOCS_URL` before building to link a custom documentation deployment. See the repository
[README](../README.md) and [local setup](../STARTUP.md) for backend and browser validation.

## Vercel

Import the GitHub repository with root directory `web/` and Node 24. The Vercel preset is enabled
when `VERCEL` is set; local and container builds retain the standard Remix server layout.
The adapter's pinned Remix peers are overridden to the same patched 2.17.5 release used by the app.
Check both the adapter build and its Node request handler when updating either package.
