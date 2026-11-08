import { DEMO_URL, DOCS_URL, REPO_URL } from "./links";

export default function Navbar() {
  return (
    <header className="border-b border-mmx-line">
      <div className="mx-auto flex h-16 max-w-[1312px] items-center gap-8 px-6 text-[15px] font-medium">
        <a
          href="/"
          className="font-display text-[25px] font-semibold tracking-[-0.01em]"
        >
          MMX
        </a>
        <div className="ml-auto flex items-center gap-6">
          <a
            href={REPO_URL}
            target="_blank"
            rel="noreferrer"
            className="hidden hover:text-mmx-periwinkle md:block"
          >
            GitHub
          </a>
          <a href={DEMO_URL} className="hover:text-mmx-periwinkle">
            Demo
          </a>
          <a
            href={DOCS_URL}
            target="_blank"
            rel="noreferrer"
            className="inline-flex h-10 items-center rounded-lg bg-white px-4 text-mmx-ink transition-colors hover:bg-mmx-periwinkle"
          >
            Read the docs
          </a>
        </div>
      </div>
    </header>
  );
}
