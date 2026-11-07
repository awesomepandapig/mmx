# Website deployment

Create two Vercel projects connected to this GitHub repository. Keep the exchange services on your
own infrastructure; these projects serve the public website and documentation.

| Setting | Website | Documentation |
| --- | --- | --- |
| Root directory | `web` | `docs` |
| Framework | Remix | Other |
| Install | `npm ci` | `npm ci` |
| Build | `npm run build` | `npm run build` |
| Node | 24.x | 24.x |
| Domain | `mmx.exchange` | `docs.mmx.exchange` |

The checked-in `vercel.json` files set the build commands. Leave Output Directory at its default:
Remix uses its Vercel server bundle; Vocs emits the static Build Output API directory.

Set `VITE_DOCS_URL=https://docs.mmx.exchange` for the website. Set `DOCS_BASE_URL=https://docs.mmx.exchange`
for documentation canonical URLs and sitemap entries. Preview assets and navigation stay on the
preview hostname.

1. Import the repository twice and choose each root directory.
2. Deploy a preview branch and check `/`, `/demo`, `/getting-started`, and `/installation/helm`.
3. Assign the domains and production branch after reviewing the previews.

GitHub pushes trigger subsequent deployments. Neither project needs exchange credentials.

Local compatibility checks build with `VERCEL=1`. The website is tested through the official
Vercel Node request handler, and the documentation through `.vercel/output/static`, including its
Markdown exports and mobile navigation. These checks do not create a hosted deployment.
