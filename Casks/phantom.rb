cask "phantom" do
  version "1.1.1"
  sha256 "cea69877552e3f862416a1f5f2c4eeaaa7d399566ccc2d826fdea118c4400670"

  url "https://github.com/tedswinyar/phantom/releases/download/v1.1.1/Phantom-1.1.1.dmg"
  name "Phantom"
  desc "Disk-usage analyzer for macOS with honest physical sizes, tiered reclaim suggestions and an MCP server"
  homepage "https://github.com/tedswinyar/phantom"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :sonoma"
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
  ]
end
