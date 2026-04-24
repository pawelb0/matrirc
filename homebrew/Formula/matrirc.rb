class Matrirc < Formula
  desc "Local IRC server backed by Matrix"
  homepage "https://github.com/pawelb0/matrirc"
  url "https://github.com/pawelb0/matrirc/archive/refs/tags/v0.1.4.tar.gz"
  sha256 "4b34687e0d269541afc258de80feb00436be622520d63872a1cc1ee7eba0fc4a"
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
