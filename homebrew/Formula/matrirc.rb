class Matrirc < Formula
  desc "Local IRC server backed by Matrix"
  homepage "https://github.com/pawelb0/matrirc"
  url "https://github.com/pawelb0/matrirc/archive/refs/tags/v0.1.1.tar.gz"
  sha256 "8c4f5a774cd44dfe6e29ff0fa4b4ba98b8f8b82106ed0867831b793321b3b049"
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
