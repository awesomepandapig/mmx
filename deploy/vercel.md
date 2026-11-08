# Website deployment

The root [`vercel.json`](../vercel.json) defines one Vercel project with two public
services. Import the repository with its root directory set to `.` and Node.js
24.x. Each package installs with `npm ci` and builds with `npm run build`.

| Service | Root | Build | Public host and paths |
| --- | --- | --- | --- |
| `docs` | `docs` | Vocs, static Build Output API | `docs.mmx.exchange/*` |
| `web` | `web` | Remix with the Vercel preset | `mmx.exchange/*`, including `/demo` |

The docs host rule runs first; the final catch-all routes the deployment hostname
and other attached domains to `web`. Attach both production domains to this
project. Documentation keeps its root-relative page and asset paths, so there is
no `/docs` prefix to maintain. A preview deployment's default hostname shows the
website; testing docs through the combined deployment requires an attached docs
hostname matching the host rule, or temporarily changing that rule on a preview
branch. Do not reuse the production docs hostname for previews.

There are no service bindings: neither website makes server-side calls to the
other. Links to documentation are browser navigation. The demo runs entirely in
the browser. Exchange services and the embedded operator console remain on your
own infrastructure and are not included in this Vercel project.

`VITE_DOCS_URL` may override the website's public documentation link at build
time; its production default is `https://docs.mmx.exchange`. `DOCS_BASE_URL` sets
documentation canonical URLs and sitemap entries at build time; its default is
also `https://docs.mmx.exchange`. These are public site settings, not bindings or
exchange credentials. Keep the production defaults for production deployments.

The package-level `web/vercel.json` and `docs/vercel.json` also support building
each site independently. Leave Output Directory unset: Remix emits its server
bundle and Vocs emits `.vercel/output` through its adapter.

## Review and verification

Confirm service names, public hosts, and the absence of bindings before deploying.
Use [Vercel Services](https://vercel.com/docs/services) and its
[host routing reference](https://vercel.com/docs/services/routing) for the current
configuration model. From the repository root, `vercel dev` runs both services
locally; `vercel dev -L` runs without cloud authentication. Test the docs host rule
with a request whose `Host` header is `docs.mmx.exchange`.

Before assigning domains, deploy a preview and check the landing page, `/demo`,
documentation home, `/getting-started`, `/installation/helm`, static assets,
`/sitemap.xml`, and documentation Markdown exports. GitHub pushes trigger later
deployments once the repository is connected.

## Cloudflare DNS

Add `mmx.exchange` and `docs.mmx.exchange` to the Vercel project's Domains screen
first. Copy the exact A/CNAME targets Vercel assigns to those domains into the
existing Cloudflare zone; do not assume a generic Vercel IP. Keep mail and other
unrelated DNS records intact. Use DNS-only records during Vercel verification.
Confirm HTTPS and both hosts before removing any previous hosting records. Add
`www.mmx.exchange` only if it should redirect to the apex domain.

Cloudflare remains the DNS provider; a nameserver migration is unnecessary.
