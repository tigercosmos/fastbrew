cask "rectangle" do
  version "1.100"
  sha256 "5cfbe9b68a558458302c5305cb7060491f68029338e2a4561c54a2981eb8622f"

  url "https://github.com/rxhanson/Rectangle/releases/download/v#{version}/Rectangle#{version}.dmg",
      verified: "github.com/rxhanson/Rectangle/"
  name "Rectangle"
  desc "Move and resize windows using keyboard shortcuts or snap areas"
  homepage "https://rectangleapp.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  conflicts_with cask: "rectangle-pro"
  depends_on macos: ">= :ventura"

  app "Rectangle.app"
  binary "#{appdir}/Rectangle.app/Contents/MacOS/Rectangle", target: "rectangle"

  uninstall quit:       "com.knollsoft.Rectangle",
            login_item: "Rectangle"

  zap trash: [
    "~/Library/Application Scripts/com.knollsoft.RectangleLauncher",
    "~/Library/Application Support/Rectangle",
    "~/Library/Caches/com.knollsoft.Rectangle",
    "~/Library/Preferences/com.knollsoft.Rectangle.plist",
  ]

  caveats do
    <<~EOS
      Rectangle needs Accessibility permission.
    EOS
  end
end
