# Publishing via homebrew tap

One-time setup of your tap repo:

```
# on github: create an empty repo named `homebrew-tap`
git clone git@github.com:pawelb0/homebrew-tap.git
cd homebrew-tap
mkdir Formula
cp ../matrirc/homebrew/Formula/matrirc.rb Formula/
git add Formula/matrirc.rb
git commit -m "Add matrirc"
git push
```

Per-release:

```
# in the matrirc repo:
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
# github auto-generates the tarball at:
#   https://github.com/pawelb0/matrirc/archive/refs/tags/v0.1.0.tar.gz

curl -sL https://github.com/pawelb0/matrirc/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256

# paste that sha256 into homebrew/Formula/matrirc.rb, bump `url` and `version`,
# then mirror to homebrew-tap:
cp homebrew/Formula/matrirc.rb ../homebrew-tap/Formula/matrirc.rb
cd ../homebrew-tap
git commit -am "matrirc v0.1.0"
git push
```

Install from the tap:

```
brew tap pawelb0/tap
brew install matrirc
```

Head install (build from `main`, no tag needed):

```
brew install --HEAD pawelb0/tap/matrirc
```

Validate the formula locally before pushing:

```
brew install --build-from-source ./Formula/matrirc.rb
brew test matrirc
brew audit --strict --new matrirc
```
