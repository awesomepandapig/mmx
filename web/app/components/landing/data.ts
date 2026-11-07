import type { LogoName } from "./BrandLogo";

export const SETUP_PROMPT =
  "Set up MMX for me. Fetch https://docs.mmx.exchange/getting-started.md and follow it.";

export interface Agent {
  name: string;
  command: string;
  logo?: LogoName;
  /** Copied as is. Without this, the setup prompt is copied. */
  copyText?: string;
  /** Official prompt link; opens a composer without submitting the prompt. */
  launchUrl?: string;
}

export const HELM_INSTALL =
  "helm upgrade --install mmx oci://ghcr.io/awesomepandapig/mmx/charts/mmx --version 0.1.0-rc.2 --namespace mmx --create-namespace --wait --timeout 10m";

export const AGENTS: Agent[] = [
  { name: "Helm", logo: "helm", command: HELM_INSTALL, copyText: HELM_INSTALL },
  {
    name: "Claude Code",
    logo: "claude",
    command: SETUP_PROMPT,
  },
  {
    name: "Cursor",
    logo: "cursor",
    command: SETUP_PROMPT,
    launchUrl: `https://cursor.com/link/prompt?text=${encodeURIComponent(SETUP_PROMPT)}`,
  },
  {
    name: "Codex",
    logo: "openai",
    command: SETUP_PROMPT,
    launchUrl: `codex://new?prompt=${encodeURIComponent(SETUP_PROMPT)}`,
  },
  { name: "Other agent", command: SETUP_PROMPT },
];

export const ENGINES: { name: string; logo: LogoName }[] = [
  { name: "Unity", logo: "unity" },
  { name: "Unreal Engine", logo: "unreal" },
  { name: "Godot", logo: "godot" },
  { name: "Bevy", logo: "bevy" },
];

// Provisional until the new benchmark run lands.
export const THROUGHPUT = {
  value: "4,000,000",
  unit: "orders per second",
  note: "One engine on an M4 Mac mini.",
};

export const ORDER_EVENTS = [
  { service: "game", color: "#3be3a6", message: "hold 950.00 gold" },
  {
    service: "api",
    color: "#ff7ac8",
    message: "POST /api/v1/orders buy 100 IRON @ 9.50",
  },
  {
    service: "auth",
    color: "#ffc53d",
    message: "player acct_7f31 named by game server",
  },
  {
    service: "engine",
    color: "#a3acff",
    message: "match order_88417, fill_01JC8Q4T",
  },
  { service: "outbox", color: "#a3acff", message: "store fill" },
  { service: "webhook", color: "#3be3a6", message: "POST /webhooks/mmx 200" },
  { service: "game", color: "#3be3a6", message: "grant 100 iron ore" },
];

export const MARKET_ITEMS = [
  { name: "Coal", bid: "4.10", ask: "4.15" },
  { name: "Iron ore", bid: "9.45", ask: "9.50" },
  { name: "Steel bar", bid: "31.20", ask: "31.40" },
  { name: "Oak log", bid: "2.05", ask: "2.10" },
  { name: "Lobster", bid: "6.80", ask: "6.95" },
];

type Tone = "good" | "bad" | "pending";

export interface LifecycleStep {
  label: string;
  detail: string;
  title: string;
  body: string;
  failed?: boolean;
  log: { source: string; message: string; result: string; tone: Tone }[];
}

export const LIFECYCLE: LifecycleStep[] = [
  {
    label: "Listed",
    detail: "POST /api/v1/orders",
    title: "The seller lists the item.",
    body: "Your game holds 100 iron ore in your database, then places the order at 9.50 gold.",
    log: [
      {
        source: "game",
        message: "hold 100 iron ore",
        result: "held",
        tone: "pending",
      },
      {
        source: "seller",
        message: "sell 100 IRON @ 9.50",
        result: "201 order_88417",
        tone: "good",
      },
    ],
  },
  {
    label: "Node lost",
    detail: "leader down",
    title: "An MMX node dies.",
    body: "The order is already on a majority of nodes, so another node takes over with the same book.",
    failed: true,
    log: [
      { source: "cluster", message: "node-1", result: "down", tone: "bad" },
      { source: "cluster", message: "node-2", result: "leader", tone: "good" },
      {
        source: "engine",
        message: "order_88417",
        result: "still open",
        tone: "good",
      },
    ],
  },
  {
    label: "Matched",
    detail: "price, then time",
    title: "The trade goes through.",
    body: "A buyer crosses at 9.50. The fill goes to the outbox.",
    log: [
      {
        source: "buyer",
        message: "buy 100 IRON @ 9.50",
        result: "201 order_88502",
        tone: "good",
      },
      {
        source: "engine",
        message: "match order_88417",
        result: "fill_01JC8Q4T",
        tone: "pending",
      },
      {
        source: "outbox",
        message: "store fill",
        result: "queued",
        tone: "good",
      },
    ],
  },
  {
    label: "Game down",
    detail: "503",
    title: "Your game server goes down.",
    body: "The fill stays in the outbox and retries with backoff until your game is back.",
    failed: true,
    log: [
      {
        source: "try 1",
        message: "POST /webhooks/mmx",
        result: "503",
        tone: "bad",
      },
      {
        source: "try 2",
        message: "POST /webhooks/mmx",
        result: "timeout",
        tone: "bad",
      },
      {
        source: "outbox",
        message: "back off",
        result: "pending",
        tone: "pending",
      },
    ],
  },
  {
    label: "Delivered",
    detail: "200",
    title: "The player gets 100 iron ore.",
    body: "Your game records the fill ID in the same transaction as the grant.",
    log: [
      {
        source: "try 3",
        message: "POST /webhooks/mmx",
        result: "200",
        tone: "good",
      },
      {
        source: "game",
        message: "INSERT applied_fills",
        result: "1 row",
        tone: "good",
      },
      {
        source: "game",
        message: "grant 100 iron ore",
        result: "done",
        tone: "pending",
      },
    ],
  },
  {
    label: "Redelivered",
    detail: "fill_id seen",
    title: "The same fill arrives twice.",
    body: "The ID is already recorded, so your game skips the grant and nothing is duplicated.",
    log: [
      {
        source: "try 4",
        message: "POST /webhooks/mmx",
        result: "200",
        tone: "good",
      },
      {
        source: "game",
        message: "INSERT applied_fills",
        result: "0 rows",
        tone: "pending",
      },
      { source: "game", message: "skip grant", result: "done", tone: "good" },
    ],
  },
];

const SERVER_ONLY = "Runs on your dedicated server, never on the client.";

export const SNIPPETS = {
  unity: {
    label: "UNITY",
    code: `// ${SERVER_ONLY}
var body = JsonUtility.ToJson(new Order {
  client_order_id = hold.Id,
  symbol = "IRON",
  side = "buy",
  type = "limit",
  size = "100",
  price = "9.50"
});

using var request = UnityWebRequest.Post(url, body, "application/json");
request.SetRequestHeader("x-player-id", player.Id);
yield return request.SendWebRequest();`,
  },
  unreal: {
    label: "UNREAL",
    code: `// ${SERVER_ONLY}
auto Request = FHttpModule::Get().CreateRequest();
Request->SetURL(OrdersUrl);
Request->SetVerb(TEXT("POST"));
Request->SetHeader(TEXT("x-player-id"), Player->Id);
Request->SetHeader(TEXT("Content-Type"), TEXT("application/json"));
Request->SetContentAsString(FString::Printf(TEXT(R"({
  "client_order_id": "%s",
  "symbol": "IRON",
  "side": "buy",
  "type": "limit",
  "size": "100",
  "price": "9.50"
})"), *Hold.Id));
Request->ProcessRequest();`,
  },
  godot: {
    label: "GODOT",
    code: `# ${SERVER_ONLY}
var body = JSON.stringify({
  "client_order_id": hold.id,
  "symbol": "IRON",
  "side": "buy",
  "type": "limit",
  "size": "100",
  "price": "9.50"
})
var headers = [
  "x-player-id: %s" % player.id,
  "Content-Type: application/json"
]
$HTTPRequest.request(ORDERS_URL, headers, HTTPClient.METHOD_POST, body)`,
  },
  webhook: {
    label: "WEBHOOK",
    code: `// Events can repeat, so apply each fill_id once, in one transaction.
app.post('/webhooks/mmx', verifySignature, async (req, res) => {
  for (const event of req.body.events) {
    await db.tx(async (tx) => {
      const { rowCount } = await tx.query(
        \`INSERT INTO applied_events (fill_id) VALUES ($1)
         ON CONFLICT DO NOTHING\`,
        [event.fill_id]
      )
      if (rowCount === 0) return

      // Pays the seller or grants the buyer.
      if (event.type === 'fill') await applyFill(tx, event)
      // Also refunds canceled orders.
      await releaseHold(tx, event.client_order_id, event.release_qty)
    })
  }

  res.sendStatus(200)
})`,
  },
};

export type SnippetKey = keyof typeof SNIPPETS;
