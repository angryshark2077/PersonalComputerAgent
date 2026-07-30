verify-structural:
    ./scripts/verify-structural.sh

verify:
    ./scripts/verify-full.sh

test-s1a-installer:
    xcodebuild test -project platform/macos/PersonalComputerAgent.xcodeproj -scheme PersonalComputerAgent -destination 'platform=macOS,arch=arm64' CODE_SIGNING_ALLOWED=NO
    python3 -m unittest scripts.tests.test_s1a_packaging -v

verify-s1a-bundle artifact:
    ./scripts/verify-s1a-bundle.sh '{{artifact}}'

build-s1a-dmg team identity version="0.1.0" output="dist/PersonalComputerAgent-S1A-arm64.dmg":
    ./scripts/build-s1a-dmg.sh --team-id '{{team}}' --identity '{{identity}}' --version '{{version}}' --output '{{output}}'
