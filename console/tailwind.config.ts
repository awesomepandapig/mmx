import type { Config } from "tailwindcss";

const token = (name: string) => `rgb(var(--${name}) / <alpha-value>)`;

export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        canvas: token("canvas"),
        surface: token("surface"),
        raised: token("raised"),
        line: token("line"),
        ink: token("ink"),
        muted: token("muted"),
        accent: token("accent"),
        "on-accent": token("on-accent"),
        good: token("good"),
        warn: token("warn"),
        bad: token("bad"),
      },
      fontFamily: {
        sans: ["Inter Variable", "ui-sans-serif", "system-ui", "sans-serif"],
        mono: ["JetBrains Mono Variable", "ui-monospace", "SFMono-Regular", "monospace"],
      },
      borderRadius: { card: "1.125rem" },
      letterSpacing: { heading: "-0.022em" },
    },
  },
} satisfies Config;
