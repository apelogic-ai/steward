import { defineConfig } from "@hey-api/openapi-ts";

export default defineConfig({
  input: "../target/steward-openapi.json",
  output: "src/api-client",
  plugins: ["@hey-api/client-fetch"],
});
