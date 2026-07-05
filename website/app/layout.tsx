import type { Metadata } from "next";
import { Nunito, Lora, JetBrains_Mono } from "next/font/google";
import "./globals.css";
import { Nav } from "@/components/Nav";
import { Footer } from "@/components/Footer";

const nunito = Nunito({
  subsets: ["latin"],
  weight: ["400", "500", "600", "700", "800", "900"],
  variable: "--font-nunito",
  display: "swap",
});

const lora = Lora({
  subsets: ["latin"],
  weight: ["400", "500", "600", "700"],
  variable: "--font-lora",
  display: "swap",
});

const mono = JetBrains_Mono({
  subsets: ["latin"],
  weight: ["400", "500", "600", "700"],
  variable: "--font-mono-face",
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL("https://wingman.dev"),
  title: {
    default: "Wingman — the self-improving, terminal-first coding agent",
    template: "%s · Wingman",
  },
  description:
    "Wingman (wingman) is a multi-provider, terminal-first, self-improving coding agent written in Rust. 73+ LLM providers behind one streaming interface, a built-in tool layer, and a learning loop that gets to know you and your projects.",
  keywords: [
    "wingman",
    "coding agent",
    "Rust CLI",
    "LLM",
    "Anthropic",
    "OpenAI",
    "terminal",
    "AI pair programmer",
  ],
  openGraph: {
    title: "Wingman — the self-improving, terminal-first coding agent",
    description:
      "Multi-provider, terminal-first, self-improving coding agent in Rust. 73+ providers, one shape.",
    type: "website",
  },
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  return (
    <html
      lang="en"
      className={`${nunito.variable} ${lora.variable} ${mono.variable}`}
    >
      <body className="grain relative min-h-screen antialiased">
        <Nav />
        <main className="relative z-10">{children}</main>
        <Footer />
      </body>
    </html>
  );
}
