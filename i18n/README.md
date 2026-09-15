# Translating Tain

One file per language. To add one, copy `en.lang`, translate the right-hand
side, and drop it in this folder — nothing else needs editing. The build picks
up every `.lang` file here, and the language appears in Settings on the next
build.

## The format

A key on its own line in square brackets, then its text until the next key:

```
[approval.wants_to_connect]
wants to connect to this PC

[admin_warning.reach]
With unattended access installed that includes the lock screen and
administrator prompts — the things a password would normally stop.
```

That is the whole syntax. There is nothing to quote and nothing to escape, so a
translation cannot break the file by containing an apostrophe, a quotation mark
or a backslash — which is the failure every quoted format has, and always on
somebody else's machine.

Lines starting with `#` are comments. Blank lines around a value are ignored;
line breaks *inside* one are kept, so a paragraph stays a paragraph and a
deliberate two-line label stays two lines.

## Placeholders

Names in braces are substituted at runtime:

```
[incoming.trusted_devices]
{n} devices
```

**Use the names, not the order.** They may appear in any order in your language,
or more than once, and a name that is not in the English original will simply be
left as written — which is how a typo shows up as `{nmae}` on screen rather than
as nothing at all.

## Plurals

English has two forms; Russian has three; Chinese has one. Where a count is
involved, the key has variants:

```
[incoming.sessions_open.one]
1 session open

[incoming.sessions_open.other]
{n} sessions open
```

Use whichever of `one`, `few`, `many` and `other` your language needs — the
rule named in `meta.plural` decides which is asked for. `other` is required, and
is what any language gets when its rule does not distinguish.

## The header every file needs

```
[meta.tag]
ru

[meta.name]
Русский

[meta.plural]
russian
```

- **tag** — the BCP 47 language tag. Matched against the system language, so
  `pt-BR` and `pt` are both fine and the more specific one wins.
- **name** — what the language is called *in that language*. Nobody looks for
  their own language under an English name.
- **plural** — one of `english` (one/other), `russian` (one/few/many/other) or
  `none` (a single form, for Chinese, Japanese, Korean, Turkish and others).
  Anything unrecognised falls back to `other` for every count, which is wrong
  but readable.

## What is deliberately not translated

- **Log files.** Every diagnosis of a user's problem so far has started with a
  log they sent. One in a language the maintainer cannot read is materially
  harder to help with, so `tracing` output stays in English. A sentence that is
  both shown and logged is looked up twice — once in the reader's language for
  the screen, once in English for the file.
- **Connection codes, endpoint ids, adapter and display names.** These are
  identifiers, and a translated one is a wrong one.
- **Library and codec names** — `libavcodec`, `Media Foundation`, `D3D11VA`,
  `H.264`. The sentence around them is translated; the names are what somebody
  searches for when the picture stops. See the `decoder.*` keys.

## The `end.*` keys are read on the other machine

Everything else in this file is written on the machine that shows it. The
`end.*` sentences are the exception: the host sends a short code and whichever
end is displaying it picks the words, so that a Russian host does not tell an
English guest anything in Russian.

Two consequences for translating them. They are addressed to somebody sitting at
the **guest**, about a machine that is **not the one in front of them** — hence
"restart Tain on the host" rather than "restart Tain". And their `{placeholders}`
carry values chosen on the far machine — a display name, a Windows error — so
keep every one of them, in whatever order your language wants.

## Missing keys

A key your file does not have falls back to English rather than showing
nothing, so an unfinished translation is usable from the first line. Run

```
cargo test -p tain-core -- i18n
```

to see exactly which keys a language is still missing.

A key that *no* file has is a different matter — it shows on screen as itself,
which is a bug in the application rather than in a translation. Run

```
cargo test -p tain-core --test i18n_keys -- --nocapture
```

to check every key the source asks for against the English file, and to list any
text in the catalogue that nothing displays.
