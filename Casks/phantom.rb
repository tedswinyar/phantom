cask "phantom" do
  version "1.1.3"
  sha256 "5fe53aab1af3b515d0a400752a7f6cbfa22f5dd4c000c7b6b9da790e9c999507"

  url "https://github.com/tedswinyar/phantom/releases/download/v#{version}/Phantom-#{version}.dmg"
  name "Phantom"
  desc "Disk-usage analyzer for macOS with honest physical sizes, tiered reclaim suggestions and an MCP server"
  homepage "https://github.com/tedswinyar/phantom"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: :sonoma
  depends_on arch: :arm64

  app "Phantom.app"
  binary "#{appdir}/Phantom.app/Contents/Helpers/phantom"
  binary "#{appdir}/Phantom.app/Contents/MacOS/phantom-mcp"
  bash_completion "#{appdir}/Phantom.app/Contents/Resources/completions/phantom.bash"
  zsh_completion "#{appdir}/Phantom.app/Contents/Resources/completions/_phantom"
  fish_completion "#{appdir}/Phantom.app/Contents/Resources/completions/phantom.fish"
  manpage "#{appdir}/Phantom.app/Contents/Resources/man/man1/phantom.1"

  zap trash: [
    "~/Library/Application Support/phantom",
    "~/Library/Logs/Phantom",
    "~/Library/Preferences/com.tedswinyar.phantom.plist",
    "~/Library/Caches/com.tedswinyar.phantom",
    "~/Library/HTTPStorages/com.tedswinyar.phantom",
  ]
end
