import {
  GetObjectCommand,
  HeadObjectCommand,
  PutObjectCommand,
  DeleteObjectCommand,
  S3Client,
} from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";

export const r2SignedUrlLifetimeSeconds = 300;

export interface R2ObjectDescriptor {
  objectKey: string;
  expectedSha256: string;
  expectedSizeBytes: number;
  expectedMimeType: string;
}

export interface R2ObjectHead {
  sizeBytes: number;
  mimeType: string;
  sha256: string;
}

export interface R2ObjectStore {
  signUpload(descriptor: R2ObjectDescriptor): Promise<{ url: string; headers: Record<string, string> }>;
  headObject(objectKey: string): Promise<R2ObjectHead | null>;
  deleteObject(objectKey: string): Promise<void>;
  signRead(objectKey: string): Promise<{ url: string; expiresAt: Date }>;
}

export interface R2Environment {
  R2_ENDPOINT?: string;
  R2_ACCESS_KEY_ID?: string;
  R2_SECRET_ACCESS_KEY?: string;
  R2_BUCKET?: string;
  R2_BUCKET_PUBLIC?: string;
}

export function createR2ObjectStore(environment: R2Environment): R2ObjectStore | undefined {
  const values = [
    environment.R2_ENDPOINT,
    environment.R2_ACCESS_KEY_ID,
    environment.R2_SECRET_ACCESS_KEY,
    environment.R2_BUCKET,
    environment.R2_BUCKET_PUBLIC,
  ];
  if (values.every((value) => value === undefined || value.length === 0)) return undefined;

  const endpoint = privateHttpsEndpoint(required(environment, "R2_ENDPOINT"));
  if (environment.R2_BUCKET_PUBLIC !== "false") {
    throw new Error("R2_BUCKET_PUBLIC must be false");
  }
  const bucket = required(environment, "R2_BUCKET");
  const client = new S3Client({
    endpoint: endpoint.toString(),
    region: "auto",
    requestChecksumCalculation: "WHEN_REQUIRED",
    credentials: {
      accessKeyId: required(environment, "R2_ACCESS_KEY_ID"),
      secretAccessKey: required(environment, "R2_SECRET_ACCESS_KEY"),
    },
  });
  return new S3R2ObjectStore(client, bucket);
}

class S3R2ObjectStore implements R2ObjectStore {
  constructor(
    private readonly client: S3Client,
    private readonly bucket: string,
  ) {}

  async signUpload(descriptor: R2ObjectDescriptor): Promise<{ url: string; headers: Record<string, string> }> {
    const url = await getSignedUrl(
      this.client,
      new PutObjectCommand({
        Bucket: this.bucket,
        Key: descriptor.objectKey,
        ContentType: descriptor.expectedMimeType,
        ContentLength: descriptor.expectedSizeBytes,
        Metadata: { sha256: descriptor.expectedSha256 },
      }),
      {
        expiresIn: r2SignedUrlLifetimeSeconds,
        unhoistableHeaders: new Set(["x-amz-meta-sha256"]),
      },
    );
    return {
      url,
      headers: {
        "content-type": descriptor.expectedMimeType,
        "content-length": String(descriptor.expectedSizeBytes),
        "x-amz-meta-sha256": descriptor.expectedSha256,
      },
    };
  }

  async headObject(objectKey: string): Promise<R2ObjectHead | null> {
    try {
      const object = await this.client.send(new HeadObjectCommand({ Bucket: this.bucket, Key: objectKey }));
      if (
        object.ContentLength === undefined
        || object.ContentType === undefined
        || object.Metadata?.sha256 === undefined
      ) {
        return null;
      }
      return {
        sizeBytes: object.ContentLength,
        mimeType: object.ContentType,
        sha256: object.Metadata.sha256,
      };
    } catch (error) {
      if (typeof error === "object" && error !== null && "$metadata" in error) {
        const metadata = error.$metadata as { httpStatusCode?: number };
        if (metadata.httpStatusCode === 404) return null;
      }
      throw error;
    }
  }

  async deleteObject(objectKey: string): Promise<void> {
    await this.client.send(new DeleteObjectCommand({ Bucket: this.bucket, Key: objectKey }));
  }

  async signRead(objectKey: string): Promise<{ url: string; expiresAt: Date }> {
    const url = await getSignedUrl(
      this.client,
      new GetObjectCommand({ Bucket: this.bucket, Key: objectKey }),
      { expiresIn: r2SignedUrlLifetimeSeconds },
    );
    return {
      url,
      expiresAt: new Date(Date.now() + r2SignedUrlLifetimeSeconds * 1000),
    };
  }
}

function required(environment: R2Environment, key: keyof R2Environment): string {
  const value = environment[key];
  if (value === undefined || value.length === 0) throw new Error(`missing required configuration: ${key}`);
  return value;
}

function privateHttpsEndpoint(value: string): URL {
  let endpoint: URL;
  try {
    endpoint = new URL(value);
  } catch {
    throw new Error("R2_ENDPOINT must be an HTTPS URL");
  }
  if (
    endpoint.protocol !== "https:"
    || endpoint.username.length > 0
    || endpoint.password.length > 0
    || endpoint.pathname !== "/"
    || endpoint.search.length > 0
    || endpoint.hash.length > 0
  ) {
    throw new Error("R2_ENDPOINT must be an HTTPS origin");
  }
  return endpoint;
}
