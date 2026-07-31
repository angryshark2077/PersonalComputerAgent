import type { Metadata } from "next";
import type { ReactNode } from "react";

import { DashboardProvider } from "./providers";

export const metadata: Metadata = {
  title: "Personal Computer Agent",
  description: "Owner Dashboard",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body>
        <DashboardProvider>{children}</DashboardProvider>
      </body>
    </html>
  );
}
