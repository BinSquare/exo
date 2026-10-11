// Mirrors the landing page's code: ink text, faint punctuation and comments,
// and blue for keys and keywords.
function exoTheme(
  name: string,
  type: "light" | "dark",
  colors: { ink: string; faint: string; key: string },
) {
  return {
    name,
    type,
    colors: {
      "editor.background": "#00000000",
      "editor.foreground": colors.ink,
    },
    settings: [
      { settings: { foreground: colors.ink } },
      {
        scope: [
          "comment",
          "punctuation.definition.comment",
          "punctuation.definition.tag",
          "punctuation.separator.key-value.mapping.yaml",
          "punctuation.definition.block.sequence.item.yaml",
          "entity.other.document.begin.yaml",
          "punctuation.definition.string",
          "markup.heading.setext",
        ],
        settings: { foreground: colors.faint },
      },
      {
        scope: [
          "entity.name.tag.yaml",
          "support.type.property-name",
          "keyword",
          "storage",
          "storage.type",
        ],
        settings: { foreground: colors.key },
      },
    ],
  };
}

export const lightTheme = exoTheme("exo-light", "light", {
  ink: "#17171a",
  faint: "#9a9aa2",
  key: "#1f4fd1",
});

export const darkTheme = exoTheme("exo-dark", "dark", {
  ink: "#e6e6e9",
  faint: "#5c5c64",
  key: "#82a8ff",
});
