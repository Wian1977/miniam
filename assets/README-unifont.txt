The miniam source code is MIT licensed; the bundled bitmap fonts below keep
their own licenses (see each section).

Bitmap font assets for the braille title
=======================================

Files (one glyph per line, "CODEPOINT:HEXDATA"):

  font_iosevka.hex  CURRENT DEFAULT. Mixed bitmap font:
                    ASCII / punctuation / symbols use Iosevka 8x16
                    (4x4 braille cells), CJK & Kana use WenQuanYi
                    Bitmap Song 12x12 / 16x16.
  font12.hex        Earlier default: WenQuanYi only (no Latin from
                    Iosevka). Kept as an alternative to swap in via
                    MINIAM_FONT_HEX.

Glyph geometry: halfwidth glyphs are 8 or 6 px wide, fullwidth 12 or 16 px.
Heights are 12 or 16 rows. Every dimension is a multiple of 2 (and of 4 for
the braille grid), so glyphs align to the braille cell grid.

Rows are stored top-to-bottom; the MSB of each 16-bit row is the leftmost
pixel. Halfwidth glyphs occupy the top bits (bits 7..2 for 6-wide, 15..8 for
8-wide); fullwidth glyphs use bits 15..4 (12-wide) or 15..0 (16-wide).

In the 16-pixel-tall caption band the 12-row glyphs sit at the top, leaving
one blank braille row below.

Coverage of the CJK portion: full printable ASCII, 20902/20992 CJK Unified
Ideographs, 169/192 Kana.

Two ranges were SUPPLEMENTED as 16-row (16x16) glyphs so that song titles in
any language display (no '?' fallback):
  - Hangul syllables 0xAC00..0xD7A3 (11172): taken from wenquanyi_12pt.bdf
    (WenQuanYi 0.5.0 16x16, GPLv2), centered into a 16x16 canvas. 16 is a
    multiple of 4 so it still aligns to the braille grid; the renderer places
    each glyph at its own height, so Chinese stays 12-tall / Korean 16-tall in
    the same line (baseline may differ slightly in mixed CN+KR titles).
  - CJK Ext A 0x3400..0x4DBF (6592): taken from GNU Unifont 16.0.03 medium,
    since the WenQuanYi 10pt/12pt sources omit it.

Net result: Hangul 100%, Ext A 100%, ASCII 100%, CJK Unified 99.6%, Kana 88%.

Sources and licences
--------------------

Iosevka
  Iosevka 8x16 bitmaps for the Latin range. SIL Open Font License 1.1.
  https://typeof.net/Iosevka/

WenQuanYi Bitmap Song 0.5.0
  wenquanyi_10pt.bdf / wenquanyi_12pt.bdf / wenquanyi_12ptb.bdf.
  GPL v2 with a font embedding exception.
  Copyright (c) 2004-2010 WenQuanYi Project, Board of Trustees and
  Qianqian Fang.

GNU Unifont 16.0.03 (2025-04-19)
  Used for the CJK Ext A supplement. Unifont is dual-licensed
  GPL v2-or-later + GNU font embedding exception AND SIL Open Font License
  1.1. Its CJK ideographs derive from WenQuanYi Bitmap Song.
  https://www.unifoundry.com/pub/unifont/unifont-16.0.03/

NetBSD wsfont bold8x16
  8-wide bold Latin bitmaps, Public Domain.
  http://ftp.netbsd.org/pub/NetBSD/NetBSD-current/src/sys/dev/wsfont/bold8x16.h
