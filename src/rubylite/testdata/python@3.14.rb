class PythonAT314 < Formula
  desc "Interpreted, interactive, object-oriented programming language"
  homepage "https://www.python.org/"
  url "https://www.python.org/ftp/python/3.14.2/Python-3.14.2.tar.xz"
  sha256 "5d3e4a4ffd7d6b9f5f0dd8f2c35c5ed1f2e5a2a6b1c7d8e9f0a1b2c3d4e5f607"
  license "Python-2.0"
  revision 1
  version_scheme 2

  livecheck do
    url "https://www.python.org/ftp/python/"
    regex(%r{href=.*?v?(3\.14(?:\.\d+)*)/?["' >]}i)
  end

  bottle do
    root_url "https://ghcr.io/v2/homebrew/core"
    rebuild 3
    sha256 cellar: :any,                 arm64_tahoe:   "aaaa000000000000000000000000000000000000000000000000000000000001"
    sha256 cellar: :any,                 arm64_sequoia: "aaaa000000000000000000000000000000000000000000000000000000000002"
    sha256 cellar: :any_skip_relocation, x86_64_linux:  "aaaa000000000000000000000000000000000000000000000000000000000003"
  end

  keg_only :versioned_formula

  depends_on "pkgconf" => :build
  depends_on "openssl@3"
  depends_on "sqlite" => [:build, :test]
  depends_on macos: :ventura
  depends_on arch: :arm64

  uses_from_macos "bzip2"
  uses_from_macos "libffi", since: :sequoia
  uses_from_macos "zlib" => :build

  conflicts_with "python-build", because: "both install `python-build` binaries"

  link_overwrite "bin/python3", "lib/python3.14/*"

  def install
    system "./configure", "--prefix=#{prefix}"
  end

  def post_install
    (var/"python").mkpath
  end

  def caveats
    <<~EOS
      Python has been installed as
        #{opt_bin}/python3.14

      Unversioned symlinks have been installed into
        #{opt_libexec}/bin
    EOS
  end

  test do
    system bin/"python3.14", "-c", "import sqlite3"
  end
end
