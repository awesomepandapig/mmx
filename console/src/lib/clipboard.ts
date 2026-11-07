/** HTTP console origins may lack Clipboard API access; retain a user-gesture fallback. */
export async function copyText(
  text: string,
  clipboard: Pick<Clipboard, "writeText"> | null | undefined = navigator.clipboard,
  doc: Document = document,
): Promise<void> {
  if (clipboard) {
    try {
      await clipboard.writeText(text);
      return;
    } catch {
      // Some browsers expose the API but reject writes without clipboard permission.
    }
  }

  const focused = doc.activeElement;
  const selection = doc.getSelection();
  const ranges = selection
    ? Array.from({ length: selection.rangeCount }, (_, index) => selection.getRangeAt(index))
    : [];
  const field = doc.createElement("textarea");
  field.value = text;
  field.readOnly = true;
  field.style.position = "fixed";
  field.style.opacity = "0";
  field.style.pointerEvents = "none";
  doc.body.appendChild(field);
  try {
    field.select();
    if (!doc.execCommand("copy")) throw new Error("Clipboard access was denied.");
  } finally {
    field.remove();
    if (focused instanceof HTMLElement) focused.focus({ preventScroll: true });
    if (selection) {
      selection.removeAllRanges();
      for (const range of ranges) selection.addRange(range);
    }
  }
}
