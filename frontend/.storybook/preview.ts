import type { Preview } from "@storybook/react-vite";

// The app's global stylesheet — pulls in Tailwind + the base font/colors so stories
// render with the same look as the real dashboard.
import "../app/app.css";

const preview: Preview = {
  parameters: {
    layout: "fullscreen",
    controls: { matchers: { color: /(background|color)$/i, date: /Date$/i } },
  },
  // Theme toolbar. The real app is auto/device-driven (dark: keys off
  // prefers-color-scheme); this toggle just adds a `.dark` class + sets color-scheme
  // on the story so dark mode is previewable here regardless of the dev's OS.
  globalTypes: {
    theme: {
      description: "Preview theme (the app itself auto-follows the OS)",
      defaultValue: "light",
      toolbar: {
        title: "Theme",
        icon: "circlehollow",
        items: [
          { value: "light", title: "Light", icon: "sun" },
          { value: "dark", title: "Dark", icon: "moon" },
        ],
        dynamicTitle: true,
      },
    },
  },
  decorators: [
    (Story, ctx) => {
      const dark = ctx.globals.theme === "dark";
      if (typeof document !== "undefined") {
        const root = document.documentElement;
        root.classList.toggle("dark", dark);
        root.style.colorScheme = dark ? "dark" : "light";
        document.body.style.background = dark ? "#0b0f1a" : "#f4f5f7";
      }
      return Story();
    },
  ],
};

export default preview;
