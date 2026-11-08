import type { MetaDescriptor } from "@remix-run/node";

export function pageMeta(
  title: string,
  description: string,
  path = "/",
): MetaDescriptor[] {
  const url = new URL(path, "https://mmx.exchange").href;
  return [
    { title },
    { name: "description", content: description },
    { tagName: "link", rel: "canonical", href: url },
    { property: "og:type", content: "website" },
    { property: "og:site_name", content: "MMX" },
    { property: "og:title", content: title },
    { property: "og:description", content: description },
    { property: "og:url", content: url },
    { name: "twitter:card", content: "summary" },
    { name: "twitter:title", content: title },
    { name: "twitter:description", content: description },
  ];
}
