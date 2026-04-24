class Matrirc < Formula
  desc "Local IRC server backed by Matrix"
  homepage "https://github.com/pawelb0/matrirc"
  url "https://github.com/pawelb0/matrirc/archive/refs/tags/v0.1.3.tar.gz"
  sha256 "36bdae78b00ee8ddfe53bae9d81768c2081abe992b4e9c35e32a9fe18b08a153"
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
