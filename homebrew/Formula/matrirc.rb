class Matrirc < Formula
  desc "Local IRC server backed by Matrix"
  homepage "https://github.com/pawelb0/matrirc"
  url "https://github.com/pawelb0/matrirc/archive/refs/tags/v0.2.2.tar.gz"
  sha256 "d91fa11f8614235ed09a9c5ece3ab2e5358e8505135d0c1f8ce54ad7dba25178"
  license "GPL-3.0-or-later"
  head "https://github.com/pawelb0/matrirc.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")
  end

  test do
    assert_match "matrirc", shell_output("#{bin}/matrirc --help")
  end
end
