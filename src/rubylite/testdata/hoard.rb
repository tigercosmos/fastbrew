class Hoard < Formula
  desc "Tiny key-value daemon with a service block"
  homepage "https://example.com/hoard"
  license any_of: ["MIT", "Apache-2.0"]
  version "2.7.0"

  on_macos do
    on_arm do
      url "https://example.com/hoard/v#{version}/hoard-#{version}-darwin-arm64.tar.gz"
      sha256 "1111111111111111111111111111111111111111111111111111111111111111"
    end
    on_intel do
      url "https://example.com/hoard/v#{version}/hoard-#{version}-darwin-x86_64.tar.gz"
      sha256 "2222222222222222222222222222222222222222222222222222222222222222"
    end
    depends_on "openssl@3"
  end

  on_linux do
    url "https://example.com/hoard/v#{version}/hoard-#{version}-linux-x86_64.tar.gz"
    sha256 "3333333333333333333333333333333333333333333333333333333333333333"
    depends_on "glibc"
  end

  head "https://github.com/example/hoard.git", branch: "main"

  keg_only "it shadows the system hoard"

  bottle do
    root_url "https://example.com/bottles"
    rebuild 2
    sha256 arm64_tahoe:  "4444444444444444444444444444444444444444444444444444444444444444"
    sha256 "5555555555555555555555555555555555555555555555555555555555555555" => :x86_64_linux
  end

  service do
    run [opt_bin/"hoard", "--config", etc/"hoard.conf"]
    run_type :immediate
    keep_alive successful_exit: false
    environment_variables PATH: std_service_path_env, HOARD_HOME: var/"hoard"
    working_dir var
    log_path var/"log/hoard.log"
    error_log_path var/"log/hoard.err.log"
    process_type :background
    require_root true
    stop_timeout 30
  end

  def install
    bin.install "hoard"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hoard --version")
  end
end
