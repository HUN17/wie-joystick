import webpack from "webpack";
import { merge } from "webpack-merge";
import { sentryWebpackPlugin } from "@sentry/webpack-plugin";

// @ts-ignore: allowImportingTsExtensions
import commonConfig from "./webpack.config.common.ts";

// Writes precache.json listing the emitted files, which public/sw.js caches for offline use.
class PrecacheManifestPlugin {
  apply(compiler: webpack.Compiler) {
    compiler.hooks.thisCompilation.tap("PrecacheManifestPlugin", compilation => {
      compilation.hooks.processAssets.tap(
        { name: "PrecacheManifestPlugin", stage: webpack.Compilation.PROCESS_ASSETS_STAGE_REPORT },
        () => {
          const files = compilation.getAssets()
            .map(asset => asset.name)
            .filter(name => !name.endsWith(".map") && !["_headers", "404.html", "sw.js"].includes(name));
          compilation.emitAsset("precache.json", new webpack.sources.RawSource(JSON.stringify(files)));
        },
      );
    });
  }
}

const config: webpack.Configuration = merge(commonConfig, {
  mode: "production",
  devtool: "hidden-source-map",
  plugins: [
    new PrecacheManifestPlugin(),
    sentryWebpackPlugin({
      // Fail the build on upload errors, which the plugin otherwise only logs.
      errorHandler(error) { throw error; },
      sourcemaps: {
        filesToDeleteAfterUpload: ["wie-web/dist/**/*.map"],
      },
    }),
  ],
});

export default config;
