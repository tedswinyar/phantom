cask "phantom" do
  version "1.1.2"
  sha256 "31b328119edc84ef3386285aa2bf32a61cfe3db1682a62908a9f20321d7cb329"

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
