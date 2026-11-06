const token = (name) => `rgb(var(--${name}) / <alpha-value>)`;

// The landing page's tokens, by the same names where it has them. `src/index.css` sets the values.
/** @type {import("tailwindcss").Config} */
module.exports = {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        canvas: token("canvas"),
        surface: token("surface"),
        raised: token("raised"),
        deep: token("deep"),
        line: token("line"),
        hairline: token("hairline"),
        ink: token("ink"),
        muted: token("muted"),
        faint: token("faint"),
        accent: token("accent"),
        brand: token("brand"),
        solid: token("solid"),
        "on-solid": token("on-solid"),
        "solid-hover": token("solid-hover"),
        good: token("good"),
        warn: token("warn"),
        bad: token("bad"),
        bid: token("bid"),
        ask: token("ask"),
        "bid-text": token("bid-text"),
        "ask-text": token("ask-text"),
      },
      fontFamily: {
        body: ['"Rethink Sans Variable"', "system-ui", "sans-serif"],
        display: ['"Funnel Display Variable"', "system-ui", "sans-serif"],
        code: ['"JetBrains Mono Variable"', "ui-monospace", "monospace"],
      },
      letterSpacing: { heading: "-0.02em" },
    },
  },
};
