# Recipes

A **recipe** is a small, reusable command that an AI agent writes once and you run yourself. It is
data only: a form (a flat JSON Schema object) and an operation template. There is no code, no
expressions and no HTML in it. A recipe cannot read your disk, run anything, or approve anything.

1. The agent proposes a recipe over `tpfs mcp` (`recipe_propose`).
2. You review it, narrow its folders if you like, and install it or reject it: with
   `tpfs recipe install <id>` (or in `tpfs watch`), or in a front end such as troppau.
3. Whenever you want, you run it on entries you choose (`tpfs recipe run <recipe> <entry>…
   --set field=value`, or a selection in the front end). Each run expands
   into an ordinary [plan](plan-format.md), which you review like any other. The plan is journaled
   and can be undone.

The agent never sees what you select or what a run did. File names cannot reach it through a
recipe, so they cannot carry instructions to it (prompt injection).

## An example

```json
{
  "format": "tpfs-recipe/1",
  "id": "receipts-to-accounting",
  "version": 1,
  "title": "Receipts to accounting",
  "scope": { "roots": ["~/Downloads", "~/Accounting"] },
  "input": { "kind": "file", "glob": ["*.pdf", "*.jpg", "*.png"], "max": 200 },
  "form": {
    "type": "object",
    "properties": {
      "target": { "type": "string", "format": "x-tpfs-dir", "title": "Accounting folder", "default": "otherPanel" },
      "supplier": { "type": "string", "title": "Supplier", "minLength": 1, "maxLength": 40 }
    },
    "required": ["target", "supplier"]
  },
  "action": {
    "op": "move",
    "to": "{form.target}/{item.mtime:%Y}/{item.mtime:%m}/{form.supplier|ascii|slug}_{item.mtime:%Y-%m-%d}_{n:02}.{item.ext|lower}",
    "on_conflict": "number"
  }
}
```

Run on `invoice.PDF` and `receipt.jpg` (both from 3 October 2026), with the supplier set to
`Café Müller`, it gives this plan:

```text
MKDIR   /home/you/Accounting/2026
MKDIR   /home/you/Accounting/2026/10
MOVE    /home/you/Downloads/invoice.PDF  ->  /home/you/Accounting/2026/10/Cafe_Muller_2026-10-03_01.pdf
MOVE    /home/you/Downloads/receipt.jpg  ->  /home/you/Accounting/2026/10/Cafe_Muller_2026-10-03_02.jpg
```

The supplier's name becomes a safe file name: `ascii` drops the diacritics and `slug` turns the
space into `_`. The extension is lowercased, and `n` numbers the files in the order you selected
them.

The plan's reason is written by tpfs, never by the recipe's author:
`Recipe "Receipts to accounting" v1: move 2 files`.

## The format

| Key | Meaning |
|---|---|
| `format` | always `tpfs-recipe/1` |
| `id` | `[a-z0-9-]{3,48}`; an update keeps the id |
| `version` | starts at 1 and grows with every update |
| `title`, `description` | plain text, at most 60 and 300 characters |
| `scope.roots` | 1–4 absolute folders; `~` is your home folder |
| `input` | `kind` (`file` or `dir`), `glob` (name patterns with `*` and `?`, case ignored), `min` (default 1), `max` (default and at most 5000) |
| `form` | a flat object: `type: "object"`, `properties`, `required` |
| `action` | `op` (`move`, `copy` or `rename`), `to` (the template), `on_conflict` (`number` or `stop`, default `stop`) |

Unknown keys are refused everywhere. Every error names the key it concerns, such as `action.to`
or `form.properties.supplier.maxLength`.

### Form fields

| Type | Keywords |
|---|---|
| `string` | `minLength`, `maxLength`, `pattern` (linear-time regular expressions), `enum` / `enumNames`, `format: "date"` |
| `integer`, `number` | `minimum`, `maximum` |
| `boolean` | none |

Every field may have a `title` and a `description`. Three keys are only hints for the front end,
and tpfs ignores them: `default` (for example `"otherPanel"`), `x-tpfs: { "remember": true }`, and
`format: "x-tpfs-dir"`, which marks a folder field. A folder field's value must lie inside the
scope. Nested objects, arrays and conditional keywords are refused. A field that the template
uses must be listed in `required`.

### Templates

`to` is literal text with substitutions `{source[:format][|filter…]}`. A `/` separates folders.

| Part | Values |
|---|---|
| Sources | `item.name`, `item.stem`, `item.ext`, `item.mtime`, `today`, `n` (the position in the selection, from 1), `form.<field>` |
| Formats | `%Y %m %d %H %M` for `item.mtime`; `%Y %m %d` for `today` and date fields (the default is `%Y-%m-%d`); `:0N` (as in `:03`) for `n` and integer fields |
| Filters | `lower`, `trim`, `ascii` (strips diacritics, so `Café Müller` becomes `Cafe Muller`), `slug` (spaces and characters that are unsafe in names become `_`, and repeated `_` are collapsed) |

- **`move` and `copy`** targets are paths. They may start with `{item.dir}` or a folder field
  such as `{form.target}`; otherwise they start in the entry's own folder.
- **A `rename` target** is a name only.

## What a run guarantees

- **A value never creates a folder.** A `/` or `\` inside a substituted value becomes `_`. Every
  produced name is checked: it is not empty, not `.` or `..`, and has no control or invisible
  characters. It is not a Windows device name such as `CON` or `NUL`, does not end in a dot or a
  space, and is at most 255 bytes long.
- **Everything stays inside the scope.** If any selected entry lies outside the scope, the whole
  run is refused; nothing is silently dropped. This uses the scope as you confirmed it at
  install, which may be narrower than the recipe's own.
- **Only `move`, `copy` and `rename`.** Format 1 has no `trash` and no `rmdir`. Missing folders
  become `mkdir` steps at the start of the plan, each created once.
- **Conflicts.** A name already taken, in the plan or on disk, is handled by `on_conflict`:
  - `number` appends `_2`, `_3` … before the extension;
  - `stop` refuses the run and lists every collision.

  Names are compared with letter case ignored.
- **Determinism.** The same recipe, selection and values give the same plan, byte for byte, so
  the plan also has the same digest.

## MCP tools

| Tool | Arguments | What it does |
|---|---|---|
| `recipe_schema` | none | the format's JSON Schema, a short guide and two examples |
| `recipe_validate` | `recipe`, optional `sample` `{items: [{name, mtime}], values}` (≤ 20 invented entries) | checks the recipe; with a sample, also returns the steps a run would produce. It touches no file: the scope is used as written and every name counts as free |
| `recipe_propose` | `recipe`, optional `note` (≤ 300 characters) | puts the recipe in the inbox and returns `{id, status: "pending"}`; at most 5 recipes wait at once |
| `recipe_status` | `id` | `pending`, `installed`, `rejected` or `unknown`. A decision is remembered for a month |

No tool can install a recipe, list installed recipes or run one.

## For front ends

The `tpfs-recipe` crate has the format, the checks and the expansion:

- `parse` or `from_value` reads and checks a recipe;
- `expand(recipe, roots, items, values, today, exists)` turns it into a `Plan`.

`Item` holds an entry's path, whether it is a folder, and its modification time as a `Stamp` in
the person's local time. The crate does no I/O. Its one question about the disk, "is this path
taken?", is the `exists` callback.

`tpfs_mcp::RecipeStore` is the inbox (`<state>/pending-recipes`). It has `scan`, `get`,
`mark_installed(id, hash)` (with the hash that was shown to the person), `reject` and `status`.
Installed recipes are kept in `<state>/recipes/<recipe id>.json` (`tpfs_mcp::InstalledRecipes`).
The format is shared by every front end that uses the same state folder:

```json
{ "recipe": { … }, "hash": "<sha-256 of the reviewed recipe>", "roots": ["/home/you/Downloads"], "installed": 1795132800, "runs": 0 }
```

`roots` is the scope the person confirmed. A file whose recipe no longer matches its hash was
changed outside a front end, and it stays disabled until it is installed again. The command line
asks for a typed `yes` during a recipe's first three runs (`runs` < 3).
