# Kelpy logo

The mark combines a kelpie's horse head with a curling wave. Keep its silhouette, proportions and turquoise
palette when using it.

- [`kelpy-light.svg`](../web/public/brand/kelpy-light.svg) is the transparent vector for light backgrounds.
- [`kelpy-dark.svg`](../web/public/brand/kelpy-dark.svg) is the transparent vector for dark backgrounds.
- [`favicon.svg`](../web/public/favicon.svg) selects its neutral tones with `prefers-color-scheme`.
- [`favicon.ico`](../web/public/favicon.ico) supplies 16, 32 and 48 pixel fallbacks.

The theme variants contain the same nine paths, with no embedded raster image. Only the neutral fills change;
the turquoise fills are shared. Vectorization preserves the design closely, with small differences from the
raster reference's texture and antialiasing.

The Web UI shows the logo for its own theme, dark by default and switched from the header, independently of the
operating system's preference. The browser tab's `favicon.svg` follows the system, like the browser around it.

## Wordmark and lockup

The name is written `kelpy_`: lowercase, in Geist SemiBold (SIL Open Font License), with the font's kerning and
letter spacing of +0.02 em, followed by a turquoise bar like a terminal cursor (`#3FC0CA`, 0.5 em wide and 0.09 em
high, on the baseline, 0.06 em after the last letter). The letters are `#111312` on light backgrounds and `#ECEFF0`
on dark ones. Do not set the name in another font, capitalise it, change its spacing or recolour the cursor.

- [`kelpy-lockup-horizontal-light.svg`](kelpy-lockup-horizontal-light.svg) and
  [`kelpy-lockup-horizontal-dark.svg`](kelpy-lockup-horizontal-dark.svg): the mark with the name on its right, for
  light and dark backgrounds. The name is drawn as paths, so the files need no font. Text size is 0.885 of the
  mark's height and the gap 0.231 of it. Both use the black mark over a soft turquoise glow, which lifts it from a
  dark background; the glow is drawn into the file, so the lockup looks the same wherever it is shown.

The Web UI shows the mark alone.

## Usage infographic

The README shows four numbered cards, Copy, Run, Work and Stop, each with a short caption and a faint icon in the top
right corner (copy, terminal, chat, power), linked by small turquoise arrows.

- `kelpy-usage-light.svg` and `kelpy-usage-dark.svg`: the cards side by side, 880 wide.
- `kelpy-usage-mobile-light.svg` and `kelpy-usage-mobile-dark.svg`: the cards stacked, 420 wide, used at viewport
  widths up to 600 px.

The cards use GitHub's card and border colours for each theme and the brand turquoise for numbers, code and arrows.
Numbers and titles are set in Geist SemiBold, captions in Geist Regular and code in Geist Mono (SIL Open Font
License), converted to paths. The files contain no embedded fonts, rasters or scripts. The README selects the layout and theme through a `picture` element and supplies a text
alternative; the explanation about stopping and saved conversations remains ordinary text below the image.
