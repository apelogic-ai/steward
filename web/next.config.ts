import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "standalone",
  poweredByHeader: false,
  reactStrictMode: true,
  async headers() {
    return [
      {
        source: "/brand/logo",
        headers: [{ key: "Content-Type", value: "image/jpeg" }],
      },
    ];
  },
};

export default nextConfig;
