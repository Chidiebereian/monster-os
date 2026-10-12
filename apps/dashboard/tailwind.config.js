/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        // MonsterOS dark palette.
        obsidian: "#07090E",
        surface: "#0D111A",
        glass: "rgba(255,255,255,0.08)",
        emerald: { DEFAULT: "#10B981", light: "#34D399" },
        crimson: { DEFAULT: "#EF4444", light: "#F87171" },
        violet: { DEFAULT: "#8B5CF6", indigo: "#6366F1" },
        amber: { DEFAULT: "#F59E0B" },
      },
      fontFamily: {
        mono: ["ui-monospace", "SFMono-Regular", "Menlo", "monospace"],
      },
    },
  },
  plugins: [],
};
