export type TokenKind =
  | "plain"
  | "comment"
  | "string"
  | "number"
  | "keyword"
  | "function"
  | "property"
  | "flag"
  | "command";

export interface Token {
  kind: TokenKind;
  text: string;
}

// Group order matches KINDS. Enough of a highlighter for the short samples on this page.
const TOKEN =
  /(\/\/[^\n]*|^#[^\n]*)|('[^'\n]*'|"[^"\n]*"|`[^`\n]*`)|(\b\d+(?:\.\d+)?\b)|(\b(?:import|await|async|const|if|from|var|using|new|auto|yield|return)\b)|(\b[a-zA-Z_]\w*(?=\())|(\b[a-zA-Z_]\w*(?=\s*[:=][^=]))|(-[A-Za-z]\b)|(^curl\b)/gm;

const KINDS: TokenKind[] = [
  "comment",
  "string",
  "number",
  "keyword",
  "function",
  "property",
  "flag",
  "command",
];

export function tokenize(code: string): Token[] {
  const tokens: Token[] = [];
  let last = 0;
  for (const match of code.matchAll(TOKEN)) {
    if (match.index > last)
      tokens.push({ kind: "plain", text: code.slice(last, match.index) });
    const kind =
      KINDS[match.slice(1).findIndex((group) => group !== undefined)];
    tokens.push({ kind, text: match[0] });
    last = match.index + match[0].length;
  }
  if (last < code.length)
    tokens.push({ kind: "plain", text: code.slice(last) });
  return tokens;
}
