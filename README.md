# HexEditor

A hex editor for Rak, built on the byte primitives Rak gained in v8.2.0 and
released in [0.8.3](https://github.com/Louiml/Rak/releases) and
[0.8.4](https://github.com/Louiml/Rak/releases). Use 0.8.4 or newer.

(Rak's version numbering jumped from `v8.1.1` to `v0.8.3` at the point where the
project moved to a Cargo workspace. There is no `v0.8.2` release — the `8.2.0`
development line became `0.8.3` — so if you are matching versions against this
README, match against `0.8.3` and `0.8.4`, not `8.2.0`.)

This is the **engine and its command line**, not the GUI. The GUI is a later phase;
everything here is the part that has to be correct first, and it is the part a GUI
cannot be built without.

```
hexcore/    the editing engine: no dependencies, std only
rak/        the same operations written in Rak, on mmap and bytes
```

## What works

```console
$ cargo test -p hexcore        # 70 tests
$ cargo run -p hexcore --bin hexedit -- dump some.bin
```

```
0000  7F 45 4C 46  02 01 01 00  48 65 6C 6C  6F 2C 20 52 |.ELF....Hello,.R|
0010  61 6B 21 00  00 00 00 00  00 00 00 00  00 00 00 DE |ak!.............|
0020  AD BE EF 30  31 32 33 34  35 36 37 38  39 3A 3B 3C |...0123456789:;<|
0030  3D 3E 3F                                           |=>?|
```

Subcommands:

| | |
| --- | --- |
| `dump <file> [--width N] [--offset N] [--len N]` | rows of offset / hex / text |
| `find <file> <pattern> [--limit N]` | literal or wildcard search |
| `structure <file> <offset> --layout <f>` | decode fields at an offset |
| `scan <file> --layout <f>` | find every occurrence of a magic value |
| `patch <file> <offset> <hex> [--expect hex]` | edit and save, optionally guarded |

## The three decisions everything else follows from

**A sparse overlay, not a mutable buffer.** The original bytes are held once and
never mutated; edits go into a `BTreeMap<offset, u8>` and a read is "the overlay if
there is one, otherwise the file". Only `save()` touches the file, and it writes
contiguous runs in ascending order. So opening a 4 GB image does not need 4 GB of
RAM before you have looked at anything, and a keystroke is not a syscall.

**Reads see pending edits.** An editor that shows the file rather than what you have
done is worse than useless: you type, nothing appears, and you cannot tell whether
the key did not register or the view is stale. Every read goes through the overlay.

**A refusal is a value.** `Error` is an enum, not a `String`: out-of-bounds,
read-only, a pattern with a lone nibble, a structure whose magic does not match. Each
carries what a GUI needs to say something useful and a test needs to assert on a
reason rather than on wording.

## Searching for a byte you do not know

The search box takes hex, because you are copying out of a hex dump. It also takes
wildcards, because the byte you are hunting is by definition the one you are missing.

```
deadbeef      four literal bytes, spaces optional
7f ?? 4c 46   any byte in the second position
7f?4          top nibble 7, bottom nibble 4
```

`?` is one **nibble**, so a pattern is always an even number of them and a lone one is
refused by name. A pattern longer than the document matches nothing rather than
truncating.

Search runs over the document in 1 MiB windows that overlap by `pattern.len() - 1`, so
a match on a seam is still found and a 2 GB file does not need 2 GB of RAM. Where the
pattern has a literal byte, the scan skips to the next candidate that could satisfy it
instead of testing every offset.

## Structures

A layout is one field per line, and the same idea as Rak's `binstruct`:

```
# name: ELF header
magic=magic:7f454c46
class=u8
endian=bits:4
version=bits:4
osabi=u8
```

```
$ hexedit structure sample.bin 0 --layout elf.txt
ELF header @ 0x00000000 (7 byte(s))
  class    0x00000004  2 (0x2)
  endian   0x00000005  0 (0x0)
  version  0x00000005  1 (0x1)
  osabi    0x00000006  1 (0x1)
```

Bit fields pack within their container, which is what makes `0x45` split into
version 4 and ihl 5 rather than both reading the whole byte. Endianness is explicit
on every integer. A decode failure is a *value* — `Decoded` carries the reasons and
the offset — because real files are mostly not the structure you are asking about,
and a decoder that stops at the first short read cannot be used to hunt for
candidates. That is what `scan` is for.

## Guarded patches

`--expect` writes only if the bytes currently there are what you say they are:

```console
$ hexedit patch fw.bin 0x1f 41424344 --expect deadbeef
wrote 4 byte(s) at 0x1f
$ hexedit patch fw.bin 0x1f 41424344 --expect deadbeef
hexedit: refused: 0x1f holds 41424344, not the expected DEADBEEF
```

so a scripted patch is safe to re-run against a file that has since changed.

## The same thing in Rak

`rak/dump.rak` and `rak/find.rak` do the equivalent work in Rak on the v8.2
primitives — `mmap_open`, `mmap_size`, byte indexing, `fmt` with a real width spec,
and nibble-level wildcard matching. They run identically on both backends:

```console
$ rakc run rak/dump.rak sample.bin      # and: rakc vm rak/dump.rak sample.bin
$ rakc run rak/find.rak sample.bin "de ad be ef"
$ rakc run rak/find.rak sample.bin "7f ?? 4c 46"
```

They exist as a check on the *language*, and they are the reason three Rak bugs were
found and fixed:

* `fmt("{:02X}", 5)` printed `5`. The spec was matched by asking whether the text
  *contained* `04X`, `08X`, `x` or `X`, so exactly two widths worked and no other
  type did. There is now a real format-spec parser, shared by both backends.
* `for b in buf` **stopped at the first `0x00`**. The VM tested the element's
  truthiness instead of the loop bound, and `0x00` is the most common byte in a binary
  file — a buffer walk died at the first one, silently. `Op::Len` now makes the bound a
  comparison against a length.
* `hex_encode` over an `mmap_slice` hex-encoded the slice's *display text*
  (`"<mmap-slice 4B>"`) instead of its bytes — and a hex view renders rows of exactly
  that.

## Not built yet

* **The GUI.** No window, no cursor, no selection. `hexcore` is the engine a GUI
  would drive and the headless operations it would share, so that the interactive path
  is not a second, untested implementation.
* **`mmap` in `hexcore`.** The file is read eagerly. The overlay design already keeps
  the original bytes resident, so an OS mapping would be a second copy for no gain at
  these sizes — but a 4 GB file is read in full, which a mapping would avoid. A
  `FileStore` trait is the seam.
* **Insert and delete.** The document edits bytes in place and its length never
  changes.
* **Redo**, and undo grouping in the GUI (the engine has `coalesce_within` for it).
* **Structure editing** from a UI, and a bundled project format.
