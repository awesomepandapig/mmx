import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { chromium } from 'playwright';

// Run against built applications. CI starts both servers before invoking this script.
const web = process.env.WEB_URL ?? 'http://127.0.0.1:5193';
const docs = process.env.DOCS_URL ?? 'http://127.0.0.1:5174';
const output = process.env.SCREENSHOT_DIR ?? '.local/browser';
await mkdir(output, { recursive: true });
for (const url of [web, docs]) {
  const deadline = Date.now() + 30_000;
  while (true) {
    try { if ((await fetch(url)).ok) break; } catch { /* server starting */ }
    assert(Date.now() < deadline, `Server did not start: ${url}`);
    await new Promise(resolve => setTimeout(resolve, 250));
  }
}
const browser = await chromium.launch({
  ...(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {}),
});
try {
  for (const width of [320, 375, 390, 768, 1440]) {
    const context = await browser.newContext({ viewport: { width, height: 900 }, reducedMotion: 'reduce' });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    for (const [label, url] of [['landing', web], ['demo', `${web}/demo`], ['docs', docs]]) {
      const response = await page.goto(url);
      assert.equal(response.status(), 200, url);
      await page.locator('h1').first().waitFor();
      await page.evaluate(() => document.fonts.ready);
      const layout = await page.evaluate(() => ({
        viewport: innerWidth,
        body: document.body.scrollWidth,
        document: document.documentElement.scrollWidth,
      }));
      assert(layout.body <= width + 1 && layout.document <= width + 1,
        `${label} overflows at ${width}px: ${JSON.stringify(layout)}`);
      if (label === 'landing') {
        await page.getByRole('link', { name: 'Read the docs', exact: true }).first().waitFor();
        const tab = page.getByRole('tab').last();
        await tab.click();
        assert.equal(await tab.getAttribute('aria-selected'), 'true');
      }
      if (label === 'demo') {
        await page.getByRole('heading', { name: 'Place an order. Watch your server run it.' }).waitFor();
        await page.locator('button[type="submit"]').click();
        await page.getByRole('status').filter({ hasText: /.+/ }).first().waitFor();
      }
      await page.screenshot({ path: `${output}/${label}-${width}.png`, fullPage: true });
      if (label === 'docs') {
        const menu = page.getByRole('button', { name: 'Open menu', exact: true });
        const mobileMenu = await menu.isVisible();
        if (mobileMenu) await menu.click();
        const navigation = page.locator(mobileMenu ? '[data-base-ui-portal]' : 'nav[data-v-sidebar]');
        await navigation.locator('a:visible').filter({ hasText: /^Fill webhook$/ }).first().click();
        await page.waitForURL('**/guides/webhooks');
        await page.locator('h1').filter({ hasText: /webhook/i }).waitFor();
      }
    }
    assert.deepEqual(errors, [], `Browser errors at ${width}px`);
    await context.close();
    console.log(`PASS landing, simulation interaction, docs: ${width}px`);
  }
  // Check real static routing, published Markdown, and machine-readable documentation.
  for (const path of ['/', '/getting-started', '/guides/markets', '/guides/webhooks',
    '/guides/authentication', '/guides/architecture', '/guides/production', '/api/rest', '/api/websocket',
    '/installation/local', '/installation/helm', '/installation/source', '/installation/kind', '/status']) {
    const response = await fetch(new URL(path, docs));
    assert.equal(response.status, 200, path);
    assert.match(response.headers.get('content-type'), /text\/html/);
    const markdown = await fetch(new URL(path === '/' ? '/index.md' : `${path}.md`, docs));
    assert.equal(markdown.status, 200, `${path} Markdown`);
    assert((await markdown.text()).length > 100);
  }
  for (const path of ['/llms.txt', '/llms-full.txt', '/sitemap.xml']) {
    assert.equal((await fetch(new URL(path, docs))).status, 200, path);
  }
  assert.equal((await fetch(new URL('/does-not-exist', docs))).status, 404);
  console.log('PASS static documentation pages, Markdown exports, LLM indexes, and 404');
} finally {
  await browser.close();
}
