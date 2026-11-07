import { PANEL, SECTION } from "./layout";
import {
  DEMO_URL,
  DOCS_URL,
  ISSUES_URL,
  REPO_URL,
  SECURITY_URL,
} from "./links";

const COLUMNS = [
  {
    title: "Product",
    links: [
      { href: DOCS_URL, label: "Docs" },
      { href: DEMO_URL, label: "Demo" },
      { href: "/#how-it-works", label: "How it works" },
      { href: "/#api", label: "Integration" },
    ],
  },
  {
    title: "Project",
    links: [
      { href: REPO_URL, label: "GitHub" },
      { href: ISSUES_URL, label: "Issues" },
      { href: SECURITY_URL, label: "Security" },
    ],
  },
];

export default function Footer({ showCta = true }: { showCta?: boolean }) {
  return (
    <>
      {showCta ? (
        <section className={`${SECTION} pb-16 lg:pb-24`}>
          <div
            className={`flex flex-wrap items-center justify-between gap-6 ${PANEL}`}
          >
            <h2 className="font-display text-[clamp(2.8rem,6vw,5rem)] font-medium leading-none tracking-[-0.02em]">
              Open the market.
            </h2>
            <div className="flex flex-wrap gap-2">
              <a
                href={DEMO_URL}
                className="inline-flex h-[50px] items-center rounded-lg border border-mmx-line px-6 text-[17px] font-medium transition-colors hover:border-mmx-periwinkle"
              >
                Demo
              </a>
              <a
                href={DOCS_URL}
                target="_blank"
                rel="noreferrer"
                className="inline-flex h-[50px] items-center rounded-lg bg-mmx-pink px-6 text-[17px] font-semibold text-mmx-ink transition-colors hover:bg-[#ff7ac8]"
              >
                Read the docs
              </a>
            </div>
          </div>
        </section>
      ) : null}
      <footer className="border-t border-mmx-line py-12 text-sm text-mmx-mute">
        <div className="mx-auto grid max-w-[1312px] grid-cols-2 gap-6 px-6 md:grid-cols-[2fr_1fr_1fr]">
          <p className="col-span-2 md:col-span-1">
            MMX is open source under MIT/Apache-2.0.
          </p>
          {COLUMNS.map(({ title, links }) => (
            <div key={title}>
              <b className="mb-1.5 block font-medium text-white">{title}</b>
              {links.map(({ href, label }) => (
                <a
                  key={label}
                  href={href}
                  className="block py-0.5 hover:text-white"
                >
                  {label}
                </a>
              ))}
            </div>
          ))}
        </div>
      </footer>
    </>
  );
}
