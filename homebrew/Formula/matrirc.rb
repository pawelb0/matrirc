class Matrirc < Formula
  desc "Local IRC server backed by Matrix"
  homepage "https://github.com/pawelb0/matrirc"
  url "https://github.com/pawelb0/matrirc/archive/refs/tags/v0.1.6.tar.gz"
  sha256 "bccd92ee0fadfb56252b61da2ac096cdfa814d94346883f1af4143ef0eeedd4c"
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
