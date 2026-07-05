import type { Metadata } from "next";
import { Nunito, Lora, JetBrains_Mono } from "next/font/google";
import "./globals.css";
import { DottedWaves } from "@/components/DottedWaves";

const nunito = Nunito({
  subsets: ["latin"],
  variable: "--font-nunito",
  display: "swap",
});

const lora = Lora({
  subsets: ["latin"],
  variable: "--font-lora",
  display: "swap",
});

const mono = JetBrains_Mono({
  subsets: ["latin"],
  variable: "--font-mono-face",
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL("https://wingman.dev"),
  title: "Wingman — Coming soon",
  description:
    "Wingman is a Rust-native, terminal-first AI coding agent with a self-improving learning loop, 73+ providers, and an autonomous pilot mode. Launching soon.",
  keywords: [
    "Wingman",
    "AI coding agent",
    "Rust",
    "terminal",
    "TUI",
    "LLM",
    "pilot mode",
    "autonomous agent",
    "coming soon",
  ],
  authors: [{ name: "Vedant Nimbarte" }],
  openGraph: {
    title: "Wingman — Coming soon",
    description: "The self-improving, terminal-first coding agent. Launching soon.",
    url: "https://wingman.dev",
    siteName: "Wingman",
    type: "website",
  },
  twitter: {
    card: "summary_large_image",
    title: "Wingman — Coming soon",
    description: "The self-improving, terminal-first coding agent. Launching soon.",
  },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html
      lang="en"
      className={`${nunito.variable} ${lora.variable} ${mono.variable}`}
    >
      <body className="grain min-h-screen antialiased">
        <div className="relative z-[2]">{children}</div>
        <DottedWaves />
      </body>
    </html>
  );
}
