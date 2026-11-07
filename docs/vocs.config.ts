import { defineConfig } from "vocs/config";

export default defineConfig({
  title: "MMX",
  titleTemplate: "%s · MMX",
  description: "MMX documentation: self-hosted exchange infrastructure for MMO economies.",
  baseUrl: process.env.DOCS_BASE_URL ?? "https://docs.mmx.exchange",
  head: { base: false },
  iconUrl: "/favicon.svg",
  accentColor: "light-dark(#2436f0, #a3acff)",
  renderStrategy: "full-static",
  topNav: [
    { text: "Home", link: "https://mmx.exchange" },
    { text: "GitHub", link: "https://github.com/awesomepandapig/mmx" },
  ],
  sidebar: [
    {
      text: "Installation",
      items: [
        { text: "Helm", link: "/installation/helm" },
        { text: "Local Docker", link: "/installation/local" },
        { text: "Local Kubernetes", link: "/installation/kind" },
        { text: "From source", link: "/installation/source" },
      ],
    },
    { text: "Getting started", link: "/getting-started" },
    { text: "Feature status", link: "/status" },
    {
      text: "Guides",
      items: [
        { text: "Architecture", link: "/guides/architecture" },
        { text: "Markets", link: "/guides/markets" },
        { text: "Fill webhook", link: "/guides/webhooks" },
        { text: "Authentication", link: "/guides/authentication" },
        { text: "Production operations", link: "/guides/production" },
      ],
    },
    {
      text: "API",
      items: [
        { text: "REST", link: "/api/rest" },
        { text: "WebSocket", link: "/api/websocket" },
      ],
    },
  ],
});
