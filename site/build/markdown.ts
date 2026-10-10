/**
 * A small Markdown renderer for the repository's own documents (the FAQ,
 * privacy and security notes, release notes). It covers only the subset
 * those files use, so the site needs no Markdown dependency.
 *
 * Responsibilities:
 * - Escape everything first, then add inline code, bold and links.
 * - Render headings, bullet lists (with indented continuations), tables and
 *   paragraphs.
 * - Split a document into `##` sections and parse pipe tables, so pages can
 *   pick out exactly the part they show.
 */

/** Escapes text for use in HTML content and attribute values. */
export const esc = (s: string): string => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

/**
 * Maps a link target as written in the source document to the URL the site
 * should use, e.g. a relative `PRIVACY.md` to its page on GitHub.
 */
export type LinkResolver = (href: string) => string;

/** Options shared by the renderers. */
export interface MarkdownOptions {
  /** Resolves relative link targets; absolute http(s) links are kept as written. */
  resolve?: LinkResolver;
  /** Heading level that `#`-headings of any depth render as. Defaults to 3. */
  heading?: number;
}

/**
 * Renders inline Markdown: code spans, bold and links, on escaped text.
 *
 * @param text - One line or paragraph of Markdown.
 * @param opts - Link resolution.
 * @returns HTML.
 */
export function inline(text: string, opts: MarkdownOptions = {}): string {
  const codes: string[] = [];
  // NOTE: code spans are lifted out first so `**` or `[x](y)` inside them stay literal.
  const lifted = esc(text).replace(/`([^`]+)`/g, (_, code: string) => `\u0000${codes.push(code) - 1}\u0000`);
  return lifted
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label: string, href: string) => {
      const raw = href.replace(/&#38;/g, "&");
      const url = /^https?:\/\//.test(raw) ? raw : (opts.resolve?.(raw) ?? raw);
      return `<a class="link" href="${esc(url)}">${label}</a>`;
    })
    .replace(/\u0000(\d+)\u0000/g, (_, i: string) => `<code>${codes[Number(i)] ?? ""}</code>`);
}

/** Splits one pipe-table row into trimmed cells. */
const cells = (line: string): string[] =>
  line
    .trim()
    .replace(/^\||\|$/g, "")
    .split(/(?<!\\)\|/)
    .map((c) => c.trim());

/** A parsed pipe table: header cells and body rows, as Markdown source. */
export interface Table {
  head: string[];
  rows: string[][];
}

/**
 * Parses the first pipe table in a Markdown fragment.
 *
 * @param md - Markdown containing a table.
 * @returns The table, or `null` if there is none.
 */
export function parseTable(md: string): Table | null {
  const lines = md.split(/\r?\n/);
  const start = lines.findIndex((l, i) => l.trim().startsWith("|") && /^\s*\|?\s*:?-{3,}/.test(lines[i + 1] ?? ""));
  if (start < 0) return null;
  const rows: string[][] = [];
  for (const line of lines.slice(start + 2)) {
    if (!line.trim().startsWith("|")) break;
    rows.push(cells(line));
  }
  return { head: cells(lines[start] ?? ""), rows };
}

/**
 * Renders a table as accessible HTML: column headers, and the first cell of
 * each row as its row header.
 */
export function renderTable(table: Table, opts: MarkdownOptions = {}, className = "table"): string {
  const head = table.head.map((h) => `<th scope="col">${inline(h, opts)}</th>`).join("");
  const body = table.rows
    .map((r) => `<tr>${r.map((c, i) => (i === 0 ? `<th scope="row">${inline(c, opts)}</th>` : `<td>${inline(c, opts)}</td>`)).join("")}</tr>`)
    .join("");
  return `<div class="${className}"><table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`;
}

/**
 * Renders block Markdown: headings, `-` bullets with indented continuation
 * lines, pipe tables and paragraphs. Fenced code renders as `<pre>`.
 *
 * @param md - Markdown source.
 * @param opts - Link resolution and heading level.
 * @returns HTML.
 */
export function markdown(md: string, opts: MarkdownOptions = {}): string {
  const h = opts.heading ?? 3;
  const out: string[] = [];
  let list: string[] | null = null;
  let para: string[] = [];
  let table: string[] | null = null;
  let fence: string[] | null = null;
  const flushPara = () => {
    if (para.length) out.push(`<p>${inline(para.join(" "), opts)}</p>`);
    para = [];
  };
  const flushList = () => {
    if (list) out.push(`<ul>${list.map((li) => `<li>${inline(li, opts)}</li>`).join("")}</ul>`);
    list = null;
  };
  const flushTable = () => {
    const t = table && parseTable(table.join("\n"));
    if (t) out.push(renderTable(t, opts));
    table = null;
  };
  for (const line of md.split(/\r?\n/)) {
    if (fence) {
      if (/^```/.test(line)) {
        out.push(`<pre><code>${esc(fence.join("\n"))}</code></pre>`);
        fence = null;
      } else fence.push(line);
      continue;
    }
    if (/^```/.test(line)) {
      flushPara();
      flushList();
      flushTable();
      fence = [];
      continue;
    }
    if (line.trim().startsWith("|")) {
      flushPara();
      flushList();
      (table ??= []).push(line);
      continue;
    }
    flushTable();
    const heading = /^#{1,6}\s+(.*)$/.exec(line);
    const bullet = /^[-*]\s+(.*)$/.exec(line);
    if (heading) {
      flushPara();
      flushList();
      out.push(`<h${h}>${inline(heading[1] ?? "", opts)}</h${h}>`);
    } else if (bullet) {
      flushPara();
      (list ??= []).push(bullet[1] ?? "");
    } else if (list && /^\s+\S/.test(line)) {
      list[list.length - 1] += ` ${line.trim()}`;
    } else if (line.trim() === "") {
      flushPara();
      flushList();
    } else {
      flushList();
      para.push(line.trim());
    }
  }
  flushPara();
  flushList();
  flushTable();
  return out.join("");
}

/** One `##` section of a document. */
export interface Section {
  /** Heading text, as written. */
  title: string;
  /** GitHub-style anchor slug of the heading. */
  slug: string;
  /** Markdown body up to the next `##` heading. */
  body: string;
}

/**
 * Turns a heading into the anchor GitHub generates for it, so deep links
 * into the repository's documents also work on the site.
 *
 * @example
 * slug('What is "Unaccounted / system reserved"?') // "what-is-unaccounted--system-reserved"
 */
export function slug(heading: string): string {
  return heading
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}

/**
 * Splits a document into its `##` sections, in order.
 *
 * @param md - The whole document.
 * @returns Every level-2 section; text before the first one is dropped.
 */
export function sections(md: string): Section[] {
  const out: Section[] = [];
  let cur: { title: string; lines: string[] } | null = null;
  for (const line of md.split(/\r?\n/)) {
    const m = /^##\s+(.*)$/.exec(line);
    if (m) {
      if (cur) out.push({ title: cur.title, slug: slug(cur.title), body: cur.lines.join("\n").trim() });
      cur = { title: (m[1] ?? "").trim(), lines: [] };
    } else cur?.lines.push(line);
  }
  if (cur) out.push({ title: cur.title, slug: slug(cur.title), body: cur.lines.join("\n").trim() });
  return out;
}

/** Strips Markdown syntax for plain-text uses such as search indexes and meta tags. */
export function plain(md: string): string {
  return md
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1")
    .replace(/^[-*]\s+/gm, "")
    .replace(/\s+/g, " ")
    .trim();
}
