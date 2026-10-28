#!/usr/bin/env ruby
# Sign exactly the checksum bytes fastframe-update verifies. The private PEM
# stays in the process environment; only the public key belongs in the repo.
require 'digest'
require 'openssl'
require 'pathname'

class ReleaseSigningError < StandardError; end

def public_key_bytes(key)
  structure = OpenSSL::ASN1.decode(key.public_to_der)
  algorithm, bits = structure.value
  unless algorithm.value.first.oid == '1.3.101.112' && bits.value.bytesize == 32
    raise ReleaseSigningError, 'The release key must use Ed25519.'
  end
  bits.value
end

def verify_manifest_files(manifest, bytes)
  names = []
  bytes.lines.each do |line|
    match = /\A([0-9a-fA-F]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)\r?\n?\z/.match(line)
    raise ReleaseSigningError, 'Invalid checksum manifest entry.' unless match
    digest, name = match.captures
    raise ReleaseSigningError, 'Duplicate checksum manifest entry.' if names.include?(name)
    names << name
    path = manifest.dirname.join(name)
    unless path.file? && !path.symlink? && Digest::SHA256.file(path).hexdigest == digest.downcase
      raise ReleaseSigningError, "Release file does not match its checksum: #{name}"
    end
  end
  raise ReleaseSigningError, 'The release manifest is empty.' if names.empty?
end

def verify_signature(manifest, bytes, public_bytes)
  algorithm = OpenSSL::ASN1::Sequence.new([OpenSSL::ASN1::ObjectId.new('1.3.101.112')])
  subject = OpenSSL::ASN1::Sequence.new([algorithm, OpenSSL::ASN1::BitString.new(public_bytes)])
  public_key = OpenSSL::PKey.read(subject.to_der)
  signature = manifest.sub_ext(manifest.extname + '.sig').binread
  unless signature.bytesize == 64 && public_key.verify(nil, signature, bytes)
    raise ReleaseSigningError, 'Release publisher signature verification failed.'
  end
end

begin
  verify_only = ARGV.delete('--verify')
  unless ARGV.length == 2
    raise ReleaseSigningError,
          'Usage: ruby sign-mix-release.rb [--verify] CHECKSUMS_FILE PUBLIC_KEY_FILE'
  end
  manifest, key_file = ARGV.map { |argument| Pathname.new(argument).expand_path }
  encoded_key = key_file.read.strip
  unless encoded_key.match?(/\A[0-9a-fA-F]{64}\z/)
    raise ReleaseSigningError, 'The committed publisher key must contain 64 hexadecimal digits.'
  end
  public_bytes = [encoded_key].pack('H*')
  bytes = manifest.binread
  verify_manifest_files(manifest, bytes)
  unless verify_only
    private_pem = ENV.delete('MIX_UPDATE_SIGNING_KEY')
    if private_pem.nil? || private_pem.empty?
      raise ReleaseSigningError, 'MIX_UPDATE_SIGNING_KEY is not configured.'
    end
    private_key = OpenSSL::PKey.read(private_pem)
    unless public_key_bytes(private_key) == public_bytes
      raise ReleaseSigningError, 'The signing key does not match the committed publisher key.'
    end
    signature = private_key.sign(nil, bytes)
    raise ReleaseSigningError, 'Unexpected Ed25519 signature length.' unless signature.bytesize == 64
    manifest.sub_ext(manifest.extname + '.sig').binwrite(signature)
  end
  verify_signature(manifest, bytes, public_bytes)
  puts(verify_only ? 'Release signature and checksums verified.' : 'Release signed and verified.')
rescue ReleaseSigningError => error
  warn error.message
  exit 1
rescue OpenSSL::OpenSSLError, IOError, SystemCallError, TypeError, NoMethodError
  # Do not print provider exception text that could contain key material.
  warn 'Could not read or verify the release signing inputs.'
  exit 1
end
